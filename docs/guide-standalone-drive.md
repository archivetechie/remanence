# Using a standalone LTO drive

Remanence can operate one LTO drive directly. You do not need a tape library,
a changer, or a running `rem-daemon`. This is the shortest route from a drive
on a Linux workstation to a self-describing Remanence tape.

The standalone path is not a reduced tape format. It uses the same objects,
parity, journals, audit records, catalog, write fences, and terminal index as a
library deployment. The difference is media handling: you insert and remove
cartridges yourself, and each command names the drive it may open.

Remanence is alpha software. Keep another copy of every file, and test restore
on your own drive before treating a tape as useful.

<!-- code-anchor: crates/remanence-cli/src/standalone.rs crates/remanence-library/src/discovery.rs @ 7a7af81d -->
## What you need

- Linux and Rust 1.88 or newer.
- A supported standalone LTO drive exposed through a Linux SCSI generic path
  such as `/dev/sg3`.
- Membership in the `tape` group.
- `CAP_SYS_RAWIO` on the `rem` binary.
- A cartridge whose contents may be overwritten for the initialization step.

Build the release binaries:

```sh
git clone https://github.com/archivetechie/remanence
cd remanence
cargo build --release
```

Grant hardware access. Log out and back in after adding the group:

```sh
sudo usermod -aG tape "$USER"
sudo install -d -o root -g tape -m 0770 /run/lock/rem
sudo setcap cap_sys_rawio+ep "$(realpath target/release/rem)"
```

Rebuilding replaces the executable and drops its file capability, so repeat
the `setcap` command after every rebuild. If you install `rem` somewhere else,
set the capability on that installed file instead.

`/run` is normally recreated at boot. Until an installation package provides
a tmpfiles rule, recreate `/run/lock/rem` after reboot if `rem doctor` reports
that it is missing or unwritable.

Run the built binary directly in the commands below, or put
`target/release` on your `PATH`:

```sh
export PATH="$PWD/target/release:$PATH"
```

## 1. Find the drive

Discovery is read-only:

```sh
rem drives --json
rem doctor --json
```

`rem drives` lists tape endpoints. Find the entry with `kind` and `ownership`
set to `standalone`, then note its `path` and `serial`. `rem doctor` checks the
effective `tape` group, the capability on the executable that is actually
running, the device-lock directory, and the state directory.

For the rest of this guide, substitute the values from discovery:

```sh
DRIVE=/dev/sg3
DRIVE_SERIAL=HU1234ABC5
```

The path is where the drive happens to be attached now. The serial is its
durable identity. Every hardware command repeats both values:

```sh
rem --device "$DRIVE" --allow-device "$DRIVE_SERIAL" ...
```

This repetition is intentional. `--device` selects an OS path;
`--allow-device` authorizes the serial that discovery must find at that path.
If the path is later reused by another device, the command refuses it.

## 2. Understand home-mode state

On a machine without `/etc/rem/config.toml`, the first standalone command
creates a private configuration and state tree at:

```text
${XDG_STATE_HOME:-$HOME/.local/state}/rem/
```

It includes a `default` tape pool, journals, an audit log, a rebuildable SQLite
catalog, and the durable calibration state. There is no daemon to start.

The default pool uses `custom:128,4,1` parity. At the default 256 KiB block
size it protects against a contiguous loss of 1 MiB while keeping the parity
written for a small home archive modest. A library-sized archive can create a
pool with the block-size-aware default geometry instead:

```sh
rem pool new archive --scheme default
```

If `/etc/rem/config.toml` already exists, standalone commands use it. Pass
`--config <PATH>` to use another existing config file. Do not point two running
processes at the same state tree; the state lock will refuse the second owner.

Back up the state tree along with your ordinary system backups. The tape is
still self-describing, but keeping the catalog and journals makes normal
restore and interrupted-write recovery faster.

## 3. Initialize a cartridge

Insert a cartridge and wait for the drive to finish loading it. Initialization
writes at the beginning of tape. Unlike library-mode `rem tape init`, the
standalone command has no dry-run option, so check the physical cartridge
before pressing Enter.

Choose a label that is unique in your local catalog. The label is operator
metadata for a standalone cartridge; the tape UUID written at BOT is the
durable identity.

```sh
rem --device "$DRIVE" --allow-device "$DRIVE_SERIAL" \
  tape init --label HOME01L9 --generation L9
```

The command waits for ready media, checks write protection and existing BOT
content, disables drive compression, writes the bootstrap, and records the
tape in the `default` pool. Copy the `tape_uuid` from its output. You will need
that exact UUID when you finalize the tape.

If the drive reports a usable density code, Remanence checks it against
`--generation`. If it cannot determine the generation, the explicit `L9`
value supplies the missing observation. Use the generation that matches your
cartridge.

## 4. Write files

Write one file or walk a directory recursively:

```sh
rem --device "$DRIVE" --allow-device "$DRIVE_SERIAL" \
  put "$HOME/Documents"
```

Each regular file becomes one independently restorable REM-OBJECT. The result
prints an `object_id` for every committed object. Save the output or copy the
IDs you care about.

To produce stable machine-readable output:

```sh
rem --json --device "$DRIVE" --allow-device "$DRIVE_SERIAL" \
  put "$HOME/Documents" > home-write.json
```

The command writes through the same checkpointed writer used by the daemon.
It records the write session and calibration fence before the first media
write, checkpoints each committed object, and quarantines an uncertain write
instead of guessing where the drive stopped.

For encrypted objects, repeat `--recipient` with two or more REMR public-key
files:

```sh
rem --device "$DRIVE" --allow-device "$DRIVE_SERIAL" \
  put private-file --recipient primary.remr --recipient recovery.remr
```

Keep the matching private keys somewhere other than the tape.

## 5. Restore before finalizing

Pick an `object_id` from the write result and restore it while the cartridge is
still loaded:

```sh
rem --device "$DRIVE" --allow-device "$DRIVE_SERIAL" \
  get <OBJECT_ID> --out restored
```

`--out` is a destination directory. For an encrypted object, add
`--private-key <REMP>` with a matching private-key file. Compare the restored
file with its source before continuing.

This `get` path uses the local catalog. After finalization, the whole tape can
also be restored without that catalog; see [Recovery on another
machine](#recovery-on-another-machine).

## 6. Finalize and verify the tape

Objects are checkpointed as they are written, but a removable archival tape
is not complete until finalization writes its three terminal-index replicas.
Finalization is irreversible: no more objects can be appended afterward.

Set `TAPE_UUID` to the value printed by `tape init`, then generate a request
UUID. Keep the same request UUID if you retry the same finalization:

```sh
TAPE_UUID=<TAPE_UUID_FROM_INIT>
FINALIZE_REQUEST=$(cat /proc/sys/kernel/random/uuid)

rem --device "$DRIVE" --allow-device "$DRIVE_SERIAL" \
  tape finalize \
  --tape "$TAPE_UUID" \
  --acknowledge-tape "$TAPE_UUID" \
  --expected-pool default \
  --reason "home archive complete" \
  --idempotency-key "$FINALIZE_REQUEST"
```

Run a quick terminal-index check, then a full protected-content verification:

```sh
rem --device "$DRIVE" --allow-device "$DRIVE_SERIAL" tape verify
rem --device "$DRIVE" --allow-device "$DRIVE_SERIAL" tape verify --full
```

The full pass reads all data and parity protected by the sidecars and can take
hours on a large tape. Do not remove the cartridge while a command is running.
When the command exits, Remanence releases its software lock and medium-removal
guard; eject the cartridge with the drive's normal controls.

If a process was killed and the drive still refuses eject, run:

```sh
rem --device "$DRIVE" --allow-device "$DRIVE_SERIAL" tape release
```

`tape release` clears the drive's removal guard. It does not clear a durable
tape-I/O quarantine or declare an uncertain write safe.

## Recovery on another machine

A finalized tape can restore without the original SQLite catalog. Install
Remanence, discover and authorize the drive, insert the cartridge, and run:

```sh
rem --device "$DRIVE" --allow-device "$DRIVE_SERIAL" \
  tape restore --out recovered
```

This reads the on-tape bootstrap and terminal index. Add `--private-key
<REMP>` for encrypted objects. If the terminal index is unavailable, `--scan`
walks from BOT instead:

```sh
rem --device "$DRIVE" --allow-device "$DRIVE_SERIAL" \
  tape restore --scan --out recovered
```

To add a Remanence tape from another installation to the local catalog rather
than immediately restoring it, use `tape adopt` with the loaded cartridge:

```sh
rem --device "$DRIVE" --allow-device "$DRIVE_SERIAL" \
  tape adopt --label HOME01L9 --pool default
```

Adoption reads the bootstrap and records its existing UUID. It does not
rewrite the tape.

## Ownership refusals

Remanence writes to an unattached drive only when discovery can prove that no
reachable changer claims it. If another SCSI device could not be probed, the
drive may appear with `ownership: unknown`; writes then fail closed.

On a host where partial discovery is expected, pin the drive's VPD page 0x80
serial in the selected config:

```toml
[[standalone_drives]]
serial = "HU1234ABC5"
```

This is an operator assertion that the serial is a standalone drive. It does
not turn a library bay into one. An unresolved bay in a visible changer still
blocks standalone writes until its ownership can be established.

`--acknowledge-unknown-ownership` permits read-only work when ownership is
unknown. It never authorizes initialization, writes, finalization, or other
mutations. A drive without a readable serial is likewise read-only and needs
`--allow-unserialized-device`; Remanence will not write through an ephemeral
device identity.

## When to move to a library

The direct workflow is intentionally one drive and one loaded cartridge per
invocation. It does not select media, operate a changer, queue concurrent
requests, or run as a network service. Once manual cartridge handling becomes
the limiting part of your archive, keep the tapes and move the same config,
catalog, and formats behind `rem-daemon` and a configured library. The normal
library path adds robotics, pool-based media selection, long-running drive
actors, and gRPC access; it does not require a different on-tape format.
