# Using a standalone LTO drive

This tutorial is for someone with one LTO tape drive and no changer, who loads
cartridges by hand. With such a drive, which these pages call a **standalone
drive**, `rem` writes and restores tapes directly and no daemon runs. By the end
you will have set up the drive, initialized a cartridge, written files to it,
restored one, and finalized and verified the tape. The later sections are
how-to material for the situations that follow: a full tape, a second copy,
encryption, a crash, and reading the tape on another machine.

Remanence is alpha software. Keep another copy of your files and test a restore
on your drive. Standalone mode has model-backed integration tests but has not
yet run on a physical standalone drive, so expect findings that depend on the
hardware. [Tested hardware](status.md#tested-hardware) says what has been tested
and what to include in a report. The page was checked against the source at
version 0.2.0.

<!-- code-anchor: crates/remanence-cli/src/standalone.rs crates/remanence-library/src/discovery.rs @ 625f7044 -->
## What you need

- Linux, an LTO drive and a compatible rewritable cartridge.
- Remanence built and installed as described in
  [Installing Remanence](guide-install.md). There is no binary release yet.
- The host prepared for tape hardware, as in
  [Prepare the host for tape hardware](guide-install.md#prepare-the-host-for-tape-hardware):
  your account in the `tape` group, the `CAP_SYS_RAWIO` capability on `rem`,
  and the lock directory `/run/lock/rem`.

In short, the host preparation is these commands, after which you log out and
back in so that the group takes effect:

```sh
sudo usermod -aG tape "$USER"
sudo setcap cap_sys_rawio+ep /usr/local/bin/rem
sudo install -d -o root -g tape -m 0770 /run/lock/rem
```

Run `rem` as your own user, never with `sudo`. A rebuild replaces the binary and
removes its capability, so repeat `setcap` after each one. The install guide
explains each gate and how to recreate the lock directory after a reboot.

Standalone `rem put` stores one regular file per object and does not need
`bsdtar`. You need `bsdtar` (the `libarchive-tools` package on Debian and
Ubuntu) only to extract objects that contain packed wrappers.

On a host without `/etc/rem/config.toml`, `rem` runs in **home mode**: it keeps
its configuration and state under `${XDG_STATE_HOME:-$HOME/.local/state}/rem/`.
That directory must be on a local, persistent filesystem. If your home directory
is on NFS, SMB/CIFS, tmpfs, ramfs or overlayfs, `rem` refuses it as an untrusted
state volume. In that case set `XDG_STATE_HOME` to a directory on a local disk,
and set it the same way for every later `rem` command.

## 1. Set up the drive

Find the drive, save it as this host's drive, and check the host:

```sh
rem setup
rem doctor
rem drives --json
```

`rem setup` first prints the same checks as `rem doctor`. It then names the
drive it found, with its model, serial and device path, and asks
`save <MODEL> serial <SERIAL> at <PATH>? [y/N]`. If discovery could not account
for every tape device on the host, it asks a second question: whether the drive
belongs to no library on this host. Answer yes only if you know that is true.
With several drives attached, select one with `rem --device /dev/sg5 setup`.
When it saves the serial, setup prints a line such as (illustrative):

```text
saved HP Ultrium 9-SCSI serial HU1234ABC5 as this host's drive
```

`rem doctor` prints one row per check: `✓` for a pass, `!` for an advisory
warning that does not block use, and `✗` for a failure, followed by its fix.
It ends with `ready to use the drive`, or with `fix the items above` and exit
status 1. On a typical host the output looks like this (illustrative, with the
state directory row left out):

```text
!  memlock           RLIMIT_MEMLOCK soft limit=8388608 bytes; configured minimum reservoir=16777216 bytes (block size=262144 bytes, before device batch clamping); fix: raise the lock limit with ulimit -l, LimitMEMLOCK= for the daemon unit, or a security/limits.d entry for the user
✓  tape group        effective group membership
✓  capability        /usr/local/bin/rem
✓  lock directory    /run/lock/rem
✓  trusted volume    state paths support trusted durability
✓  standalone drive  1 standalone drives found; 0 drives with unknown ownership
ready to use the drive
```

The `memlock` row compares the locked-memory limit with the minimum read
buffer, 16 MiB with the home defaults (256 KiB blocks, 16 blocks per read
batch, 4 staging buffers). A lower limit does not stop anything: reads continue
in unlocked memory and print a warning that begins
`read reservoir uses unlocked buffers`.
[The memory-lock limit](guide-install.md#the-memory-lock-limit) shows how to
raise it.

In `rem drives --json`, the drive you plan to write must appear with
`"kind": "standalone"`, `"ownership": "standalone"`, a `/dev/sgN` path and a
serial.

Setup saves the serial as `drive_serial` in the `[home]` table of the home
`config.toml`. Later commands find the drive by that serial, so a changed
`/dev/sgN` number does not change which drive is authorized. The home
configuration also defines a [tape pool](reference-glossary.md#catalog-and-daemon)
called `default`, the group of tapes that writes go to. Its tapes use 256 KiB
blocks and the parity scheme `custom:128,4,1`: 128 data blocks and 4 parity
blocks per stripe, and one stripe per neighborhood (the
[glossary](reference-glossary.md#formats-and-objects) defines these terms). That
small geometry keeps the parity of a short write small; `rem pool new` with
`--scheme default` creates a pool with the larger geometry that libraries use.

The short commands on this page rely on that saved drive. To name a drive
explicitly instead, put `--device` and `--allow-device` before the verb, with
the path and serial that discovery reports:

```sh
rem --device /dev/sg5 --allow-device HU1234ABC5 tape identify
```

The saved serial does not authorize an explicitly supplied device. An explicit
`--config` or `--endpoint` sends the commands that libraries share (`put`,
`get`, `tape init` and others) to the library route instead.

## 2. Initialize a cartridge

Insert a cartridge and wait for it to load. Write `HOME01` on its shell, then
initialize it:

```sh
rem tape init --label HOME01 --generation L9
```

Initialization writes immediately; standalone `tape init` has no dry run. It
writes the **bootstrap**, the first record on the tape, which holds the tape's
UUID and how the tape is written, at the beginning of tape (BOT). The UUID is
the tape's identity. The label is free text you choose, unique in your catalog,
and it does not have to end in a generation code.

`--generation` takes the cartridge's media code, the two characters that end an
LTO barcode label: `L5` to `L9` for LTO-5 to LTO-9, or `M8` for an LTO-7
cartridge used as Type M. The parser also accepts `L1` to `L4`, and `LZ`, which
it reads as LTO-9. Give the code of your cartridge, which your drive must be
able to write. On an LTO-6 drive with an LTO-6 cartridge, for example:

```sh
rem tape init --label HOME01 --generation L6
```

When the drive reports the cartridge's recording density, Remanence records the
generation measured from it, and a `--generation` that disagrees only produces
a warning in the log. Without `--generation`, a label that ends in a media code
must agree with the measured generation, or init refuses with a message such as
`generation hint LTO-9 disagrees with measured LTO-8`. When the drive reports no
density, init uses `--generation` or the label's code, and refuses with
`unknown generation; specify --generation Lx` when it has neither.

Remanence turns drive compression off. Before it writes, init waits for the
drive to report the cartridge ready, polling every 30 seconds for up to 2.5
hours. A new LTO-9 cartridge can take an hour or more on its first load while
the drive optimizes it, so a first initialization can look like a pause.

A cartridge formatted for LTFS has two partitions, and init refuses it before
it looks for data, with a message that begins
`this cartridge has 2 partitions (LTFS formats tapes this way)`. The message
goes on to give the repartitioning commands. Repartitioning erases everything on
the cartridge, so rescue any files you need first. It needs the `mt` program
(the `mt-st` package) and the drive's non-rewinding tape node, `/dev/nstN`. To
find that node, list the drive's sysfs entry, replacing `sg5` with your drive's
`/dev/sgN` name from `rem drives --json`:

```sh
ls /sys/class/scsi_generic/sg5/device/scsi_tape/
```

The listing includes a name such as `nst0`. Use `/dev/nst0`, or the name you
found, in both commands below, then initialize again:

```sh
sudo mt -f /dev/nst0 stsetoptions can-partitions
mt -f /dev/nst0 mkpartition 0
rem tape init --label HOME01 --generation L9
```

The Linux `st` driver allows `mkpartition` only with the `can-partitions`
option set, and setting an option needs administrative privilege even when your
account can open the tape node. Setting an option that is already set does no
harm. Add `--clobber-data` only if init still reports data at BOT.

Init refuses a used cartridge by default, including a retired cartridge that
still holds its old bootstrap. The following command destroys everything on the
cartridge, including a Remanence tape whose files are still in your catalog, so
run `rem list --tape HOME01` first and rescue anything you need:

```sh
rem tape init --label HOME01 --generation L9 --clobber-data
```

Init asks you to type `CLOBBER HOME01`. For a run without a prompt, add
`--confirm-clobber "CLOBBER HOME01"`. `--clobber-data` does not bypass write
protection, the partition check, the label and identity swap checks, or the
pool checks.

<!-- code-anchor: crates/remanence-cli/src/standalone.rs crates/remanence-cli/src/standalone_writing.rs @ 625f7044 -->
## 3. Put files on tape

Write a directory to the loaded tape:

```sh
rem put "$HOME/Documents"
```

`put` walks directories recursively and prints one receipt line per file,
followed by a total. The output looks like this (illustrative):

```text
written              Documents/notes.txt  6c0d2b9a-4e1f-4a7b-8d3c-5f2e9a1b7c40  HOME01 (5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f)  2048 bytes
written              Documents/report.pdf  a7e3f1c2-9d4b-4c6e-b2a8-1f0e7d3c5b96  HOME01 (5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f)  183402 bytes
skipped_unsupported  Documents/latest (symlink)
Total: 2 written, 0 already_stored, 1 skipped, 0 refused
```

Each line gives the event, the archive path, the object UUID, and the tape's
label and UUID. A regular file is `written` or, when an identical version is
already stored, `already_stored`. Symlinks and special files are
`skipped_unsupported` and are not backed up. The `Total:` line counts the
results, and names the file that failed if one did. `--json` prints the
receipts as JSON lines followed by a summary.

Each file becomes its own **object** on tape: one tape file, ended by its own
filemark, occupying at least one tape block. A tree of many small files
therefore uses tape inefficiently; read [Object sizing](guide-object-sizing.md)
before you write one.

## 4. List the files and get one back

List what the catalog holds, then restore one file:

```sh
rem list
rem list --tape HOME01 --path-prefix Documents/
rem get Documents/notes.txt --out restored
```

`rem list` prints a table with the columns `POOL`, `PATH`, `SIZE`, `VERSIONS`,
`NEWEST OBJECT`, `TAPES` and `FINALIZED`. It reads only the local catalog and
needs no drive. Replace `Documents/notes.txt` with a path it printed. A
directory you put keeps its last path component, so
`$HOME/Documents/notes.txt` is stored as `Documents/notes.txt`.

`rem get` reads the newest committed version from the loaded tape. If the path
is known but stored on another tape, it says which tapes hold it. It stages the
data on the output filesystem and sets the recorded modification times after
the data is validated. Compare the restored file with the source.

## 5. Finalize, verify and eject

**Finalizing** writes the tape's closing index, the terminal index, three times
at the end of the tape, so that another machine can find the files without your
catalog. Each copy is a terminal replica. Finalizing permanently closes the
tape to new objects, and its only required argument is the acknowledgement of
the tape:

```sh
rem tape finalize --acknowledge-tape HOME01
rem tape verify
rem tape verify --full
```

The first verification checks the tape's structure and terminal index. `--full`
also reads all the data that the parity
[sidecars](reference-glossary.md#formats-and-objects) protect, and the parity
itself, and it can take hours. A repeated finalize uses the same automatically
generated per-tape request key, so a retry does not finalize twice.

Each command prints a JSON summary. A successful finalize reports
`"finalized": true`. `rem tape verify` reports `"status": "verified"` with the
tape's `object_count`, and `--full` reports `"verified_complete": true`. On a
tape without a usable terminal index, `rem tape verify` exits with status 1 and
`error: verification failed: no usable terminal index; not_finalized`, or
`terminal_index_unusable` in place of `not_finalized` when an index exists but
cannot be used. A full verification that does not complete exits with status 1
and `error: full verification incomplete`.

Wait for the command to exit before you eject. Use the drive's button, or the
`mt` command with the drive's non-rewinding tape node, found as in
[2. Initialize a cartridge](#2-initialize-a-cartridge):

```sh
mt -f /dev/nst0 eject
```

If an interrupted command left the drive refusing to eject, clear its
medium-removal guard:

```sh
rem tape release
```

This clears the eject guard only. A quarantine fence, described under
[After a crash or a killed write](#after-a-crash-or-a-killed-write), stays in
place.

## Where to go next

The rest of this page covers what comes after a first tape. For what each
command accepts, see the [CLI reference](reference-cli.md). For the full
verification's output, see [Full verification](guide-full-verification.md).

## When the tape fills

Put stops with a nonzero exit status. Its summary names the failed file and
prints `finalize this tape, init the next in pool default, and re-run the same
put`. Files with `written` receipts are committed. The failed file is not a
successful backup; files not yet visited remain to be written.

For an ordinary capacity refusal, finalize and eject the first tape as above.
Insert a fresh cartridge and continue in the same pool:

```sh
rem tape init --label HOME02 --generation L9 --pool default
rem put "$HOME/Documents"
```

Files already stored are skipped, including those on HOME01. Changed and
remaining files are written to HOME02. A single file never spans two
cartridges. If the failure reports uncertain I/O or a fence, follow
[After a crash or a killed write](#after-a-crash-or-a-killed-write) before you
try to finalize.

## Two copies

A pool holds one set of copies. To make an independent second set, insert
another cartridge and use a second pool:

```sh
rem pool new offsite
rem tape init --label OFFSITE01 --generation L9 --pool offsite
rem put "$HOME/Documents" --pool offsite
rem tape finalize --acknowledge-tape OFFSITE01
rem tape verify --full
```

Eject it and keep it offsite. Running `put` again in the original pool skips
unchanged files; it does not create a second copy there.

## Backing up again later

Load an open tape of the same pool, or initialize a new one if the old tape is
finalized, and run the same `put`. A changed file creates a new version. An
unchanged file is skipped only when its size and SHA-256 match the newest
committed version in that pool, so changing a file from A to B and back to A
creates three versions.

Keep the `put` receipts if you may need an exact older version. `rem list` shows
the number of versions and the newest object UUID, not every older UUID, and
`--tape HOME01` shows the newest version on that tape. For an older version,
take its object UUID from a saved receipt, load the tape named there, and run:

```sh
rem get Documents/notes.txt --version <OBJECT_UUID> --out restored-old
```

`<OBJECT_UUID>` is the object UUID from the receipt.

## After a crash or a killed write

A fence records a write whose completion is uncertain. Writes to that tape or
drive refuse until the cause is understood. Reads still work and report active
fences, so start with inspection and rescue:

```sh
rem tape quarantine list
rem tape identify
rem tape restore --scan --out rescued
```

After checking the hardware and preserving the data, release the named fence
with a description of what you checked:

```sh
rem tape quarantine release <ID> --ack "checked drive and rescued readable files"
```

Release records your acknowledgement; it does not prove that the append point
is safe. Every append checks the physical tail. `TailBeyondKnownState` means
complete data exists beyond the position your local state knows. Restore a
newer state backup, or rescue with `--scan`. Do not delete the tape logs in the
state directory to make the refusal go away.

If you deliberately discard that suffix after rescuing it, run:

```sh
rem tape supersede-tail --acknowledge-tape HOME01
```

The command lists what it discards and asks for `SUPERSEDE HOME01`. For a
non-interactive run, add `--confirm-supersede "SUPERSEDE HOME01"`. This is an
audited destructive decision, not a routine retry step.

<!-- code-anchor: crates/remanence-api/src/catalogless_restore.rs crates/remanence-cli/src/standalone.rs @ 625f7044 -->
## Recovery on another machine

A finalized tape needs nothing from the machine that wrote it. On the new
machine, build Remanence, prepare the host and run `rem setup` as above, then
insert the tape and list or restore its files:

```sh
rem tape restore --list
rem tape restore --path Documents/notes.txt --out recovered
```

Use the exact member path from the listing. Repeat `--path` for more files, or
leave it out to restore everything. For encrypted objects, add
`--private-key recovery.remp`; listing the members of an encrypted object needs
the key as well. If the terminal index is absent or cannot be used, scan the
tape from BOT instead:

```sh
rem tape restore --scan --list
rem tape restore --scan --path Documents/notes.txt --out recovered-scan
```

A restore writes each object it restores into its own `candidate-<N>/`
subdirectory of the output directory, with a `manifest.json` that records the
object's identifier, its tape-file number and the subdirectory that holds its
files (the `payload` key, normally `.`). Because standalone `put` stores one
object per file, each file lands in its own candidate directory. Several
versions of one path produce several candidates, and `--path` can still return
more than one. Inspect each `manifest.json` and choose the candidates you need.
Modification times are set after validation.

No `rem` command merges candidate directories into one tree yet. Until one
exists, this Python 3 script does it. It reads each candidate's `manifest.json`
to find the files, copies them into a new directory `merged/`, and stops instead
of choosing when two candidates hold the same path. If that happens, set
`candidates` to the directories you want and use a fresh destination:

```sh
python3 - <<'MERGE'
import json, shutil
from pathlib import Path
candidates = sorted(Path("recovered").glob("candidate-*"))
files = {}
for candidate in candidates:
    manifest = candidate / "manifest.json"
    payload = candidate / json.loads(manifest.read_text())["payload"]
    for source in payload.rglob("*"):
        if source == manifest or not source.is_file():
            continue
        relative = source.relative_to(payload)
        if relative in files:
            raise SystemExit(f"Choose one candidate for {relative}")
        files[relative] = source
output = Path("merged")
output.mkdir()  # Refuse an existing destination.
for relative, source in files.items():
    target = output / relative
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, target)
MERGE
```

`rem tape adopt` records the tape's identity in the new machine's catalog, and
nothing more. It imports no object rows, so it does not make `rem list` or
`rem get` by path work on the new machine; use the catalog-less restore above
for that. To adopt the tape into the `default` pool under its label:

```sh
rem tape adopt --label HOME01 --pool default
```

## What to keep besides the tape

A finalized plaintext tape needs nothing from the original machine to restore
its files; you still need compatible hardware and a reader. Keep the state
directory as well, because it makes listing, ordinary restores and recovery of
an interrupted write easier. Restore the whole directory to the same path.

Keep the `.remp` private key files of encrypted files apart from the tapes, and
keep the recovery key in a separate safe place from the working key. Never
restore an older state directory over a newer one without first reading about
the `TailBeyondKnownState` refusal in
[After a crash or a killed write](#after-a-crash-or-a-killed-write). While the
formats remain under review, keep copies of the source files as well; see
[What works, and what does not](status.md).

<!-- code-anchor: crates/remanence-cli/src/lib.rs crates/remanence-aead/src/lib.rs @ 625f7044 -->
## Encryption

Plaintext is the default. An encrypted write needs two to eight distinct
recipient keys, so that the loss of one private key does not lose the data, and
any one matching private key restores it. Keep a working key and a recovery key
in separate safe places.

Create a working key:

```sh
rem archive recipient keygen --out primary.remp --label primary
```

This creates `primary.remp`, the private file (mode `0600`), and
`primary.remr`, the public file for slot 0. Both paths must be new, and
`--public FILE.remr` chooses a different public path. A label holds at most 32
printable ASCII bytes. Back up the private file and keep it safe, because a lost
private file cannot be recovered. Keep the public file on the machine that
writes tapes; it can be copied without exposing the private key. SSH and age
keys cannot stand in for these
[key-file formats](reference-cli.md#recipient-key-file-formats).

Each recipient key has a slot number, and a write takes its keys in ascending
slot order. `keygen` always writes the public file for slot 0, so the recovery
key needs a public file for slot 1. `rem archive recipient derive` prints that
record as JSON, but no `rem` command writes it to a `.remr` file yet. Until one
does, a short Python 3 script decodes the record's
`canonical_public_key_base64` field into the file:

```sh
rem archive recipient keygen --out recovery.remp --label recovery
rem archive recipient derive --private-key recovery.remp --slot-index 1 > recovery-public.json
python3 - <<'KEYS'
import base64, json
from pathlib import Path
record = json.loads(Path("recovery-public.json").read_text())
Path("recovery.remr").write_bytes(base64.b64decode(record["canonical_public_key_base64"]))
KEYS
```

`keygen` also wrote a slot-0 `recovery.remr`; the script replaces it with the
slot-1 record. `rem archive recipient inspect --public-key recovery.remr` shows
`"slot_index":1` when it worked. Keep `recovery.remp` apart from both
`primary.remp` and the tapes.

With the two public files, write an encrypted file:

```sh
rem put private-file --recipient primary.remr --recipient recovery.remr
```

A plaintext `put` refuses with the receipt `refused` when the newest version of
that file is encrypted. Give the recipients again, or add
`--allow-plaintext-copy` only if you mean to store a plaintext version.

## Ownership refusals

Before it writes, `rem` must be sure that the drive belongs to no changer.
Discovery skips disks and other devices that are not tape drives or changers,
using their sysfs type, so do not join the `disk` group to fix tape discovery. A
failed probe of a tape drive or changer, or a changer bay whose drive cannot be
resolved, leaves ownership unknown. Fix that probe, or pin the serial of a drive
you know is standalone in the selected configuration:

```toml
[[standalone_drives]]
serial = "HU1234ABC5"
```

A pin cannot override a changer's claim on the drive, or an unresolved bay.
`--acknowledge-unknown-ownership` allows reads only. A drive without a readable
serial also needs `--allow-unserialized-device` for reads, and it cannot be
written. [Troubleshooting](guide-troubleshooting.md#a-standalone-drive-is-missing-or-read-only)
lists the refusal messages.
