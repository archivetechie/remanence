# Using a standalone LTO drive

With one manually loaded LTO drive, `rem` writes and restores tapes directly.
There is no daemon to start. You insert the cartridges yourself.

Remanence is alpha software. Keep another copy of your files and test a restore
on your drive. See [tested hardware](status.md#tested-hardware) for the limits
of hardware testing.

<!-- code-anchor: crates/remanence-cli/src/standalone.rs crates/remanence-library/src/discovery.rs @ 4dc2a3c6 -->
## What you need

You need Linux, a C compiler (`build-essential` on Debian/Ubuntu, `gcc` on
Fedora), Rust 1.88 or newer (rustup), an LTO drive and a compatible rewritable
cartridge. No published binary has standalone mode yet; build from source:

```sh
git clone https://github.com/archivetechie/remanence
cd remanence
cargo build --release
export PATH="$PWD/target/release:$PATH"
```

Your account needs the `tape` group and the binary needs `CAP_SYS_RAWIO`.
Use sudo for these installation steps, but never run rem with sudo:

```sh
sudo usermod -aG tape "$USER"
sudo install -d -o root -g tape -m 0770 /run/lock/rem
sudo setcap cap_sys_rawio+ep "$(realpath target/release/rem)"
```

Log out and back in after adding the group. From the directory containing your
checkout, restore the command path:

```sh
cd remanence
export PATH="$PWD/target/release:$PATH"
```

Repeat `setcap` after rebuilding; a rebuild replaces the executable and can
remove its capability. To recreate the lock directory after each reboot, put
this line in
`/etc/tmpfiles.d/rem.conf`:

```text
d /run/lock/rem 0770 root tape -
```

Standalone `put` stores one regular file per object and does not need
`bsdtar`. Install `bsdtar` (`libarchive-tools` on Debian/Ubuntu) for the local
archive walkthrough and for extracting objects that contain packed wrappers.

## 1. Set up the drive

```sh
rem setup
rem doctor
rem drives --json
```

Setup shows the drive model, serial and current device path, then asks before
saving the serial. If discovery is incomplete, it separately asks whether the
drive belongs to no library on this host. Confirm only if you know that is true.
With several drives, use `rem setup --device /dev/sg3` to select one.

On a host without `/etc/rem/config.toml`, home state lives at
`${XDG_STATE_HOME:-$HOME/.local/state}/rem/`. Setup saves `[home] drive_serial`
in its `config.toml`. Later commands rediscover the path by serial; a changed
`/dev/sgN` number does not change the authorized drive. The home config includes
a `default` pool with `custom:128,4,1` parity and a 256 KiB block size.

These short commands assume that home setup. An explicit `--config` or
`--endpoint` selects the library route for shared verbs. To select a standalone
drive explicitly, use `rem --device /dev/sg3 --allow-device SERIAL` before the
verb, replacing the path and serial with discovery's values. The saved serial
does not authorize an explicitly supplied device.

## 2. Initialize a cartridge

Insert a cartridge and wait for it to load. Write `HOME01` on its shell, then:

```sh
rem tape init --label HOME01 --generation L9
```

Initialization writes immediately; standalone init has no dry run. The label
is free text you choose, unique in your catalog. It is not the tape's identity:
that is the UUID written in the bootstrap at the beginning of tape (BOT).
The label does not have to end in a generation code.

Supported `--generation` values are `L5`, `L6`, `L7`, `L8`, `L9` and `M8`.
Use the cartridge's generation, compatible with your drive. For example, on
an LTO-6 drive with an LTO-6 cartridge, use this **instead**:

```sh
rem tape init --label HOME01 --generation L6
```

The drive's measured density wins and is recorded. Without `--generation`,
a label ending in a generation code must agree with it. Remanence disables
drive compression. LTO-9 media can take an hour or
more on their first load while the drive optimizes them.

**Cartridges formatted with LTFS.** Init refuses these with `has 2 partitions`
before checking for old data. Repartitioning erases everything on the cartridge;
rescue any files you need first. Install `mt-st` (`sudo apt install mt-st` on
Debian/Ubuntu or `sudo dnf install mt-st` on Fedora). Use the `/dev/sgN` path
from `rem drives --json` to find the matching non-rewinding tape node:

```sh
ls /sys/class/scsi_generic/sgN/device/scsi_tape/
```

Replace `sgN` with your drive's name. The listing includes the node name;
use `/dev/<the listed nstN>` (for example `/dev/nst0`). Use that
non-rewinding node in both commands below:

```sh
sudo mt -f /dev/nst0 stsetoptions can-partitions
mt -f /dev/nst0 mkpartition 0
rem tape init --label HOME01 --generation L9
```

Use your cartridge's generation in the init command. The Linux `st` driver
requires `can-partitions` for `mkpartition`; setting this option requires
administrative privilege, even when your account can access the tape node.
Setting an already enabled option is safe. Add `--clobber-data` only if init
still reports data at BOT. Run rem as your normal user.

A used cartridge is refused by default, including a retired cartridge that
still holds its old bootstrap. If its contents may be destroyed, use:

```sh
rem tape init --label HOME01 --generation L9 --clobber-data
```

Type `CLOBBER HOME01` when asked. For a non-interactive invocation, add
`--confirm-clobber "CLOBBER HOME01"`. Clobber also erases a Remanence tape whose
files are still in your catalog; run `rem list --tape HOME01` first. It does
not bypass write protection, partition, label/identity-swap or cross-pool
checks. Rescue anything you need before erasing it.

## 3. Put files on tape

```sh
rem put "$HOME/Documents"
```

Directories are walked recursively. Each regular file gets a receipt:
`written` or `already_stored`, with its object UUID and tape label/UUID.
Symlinks and special files get `skipped_unsupported`; they are not backed up.
The final `Total:` line counts the results and names any failed file.
`--json` emits receipts as JSON lines and a final result envelope.

## 4. List the files and get one back

```sh
rem list
rem list --tape HOME01 --path-prefix Documents/
rem get Documents/notes.txt --out restored
```

Replace `Documents/notes.txt` with a path printed by `rem list`. A directory
input keeps its last path component: `$HOME/Documents/notes.txt` is stored as
`Documents/notes.txt`. Listing uses only the local catalog and needs no drive.
Get reads the newest committed version on the loaded tape. It tells you which
tapes hold a path if it is known elsewhere but absent here.

Compare the restored file with the source. Get stages its data on the output
filesystem and restores recorded modification times after validation.

## 5. Finalize, verify and eject

Finalization writes three copies of the closing index, so another machine can
find the files without your catalog. It permanently closes the tape to new
objects. Only the acknowledgement is required:

```sh
rem tape finalize --acknowledge-tape HOME01
rem tape verify
rem tape verify --full
```

The first verification checks structure and the terminal index. `--full` also
checks all sidecar-protected data and parity; it can take hours. Finalize retries
use the same automatically generated per-tape request key.

Wait for the command to exit before ejecting. Use the drive button, or the
matching non-rewinding tape node. Install `mt-st` and find the drive's `nstN`
with `ls /sys/class/scsi_generic/sgN/device/scsi_tape/` as in step 2. Replace
`sgN` with the name from `rem drives --json` and `/dev/nst0` below with the
matching tape node:

```sh
mt -f /dev/nst0 eject
```

If an interrupted command leaves the medium-removal guard set:

```sh
rem tape release
```

This releases the eject guard, not a quarantine fence.

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

Already stored files skip, including files on HOME01. Changed and remaining
files write to HOME02. A single file does not span cartridges. If the failure
reports uncertain I/O or a fence, follow the crash procedure below before
trying to finalize.

## Two copies

A pool is one set of copies. To make an independent second set, insert another
cartridge and use a second pool:

```sh
rem pool new offsite
rem tape init --label OFFSITE01 --generation L9 --pool offsite
rem put "$HOME/Documents" --pool offsite
rem tape finalize --acknowledge-tape OFFSITE01
rem tape verify --full
```

Eject it and keep it offsite. Repeating put in the original pool skips unchanged
files; it does not create a second copy there.

## Backing up again later

Load an open tape in the same pool, or initialize a new one if the old tape is
finalized, then run the same put. A changed file creates a new version. An
unchanged file skips only when its size and SHA-256 match the newest committed
version in that pool. Changing A to B and back to A creates three versions.

Keep put receipts if you need to select exact older versions. `rem list` shows
version counts and the newest UUID, not every historical UUID. Filtering with
`--tape HOME01` shows the newest version on that tape. For an older version,
use its object UUID from a saved receipt, load the tape named there, and run:

```sh
rem get Documents/notes.txt --version <OBJECT_UUID> --out restored-old
```

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
newer state backup, or rescue with `--scan`. Do not delete journals to make the
refusal go away.

If you deliberately discard that suffix after rescuing it, run:

```sh
rem tape supersede-tail --acknowledge-tape HOME01
```

The command lists what it discards and asks for `SUPERSEDE HOME01`. For a
non-interactive run, add `--confirm-supersede "SUPERSEDE HOME01"`. This is an
audited destructive decision, not a routine retry step.

## Recovery on another machine

Build Remanence, set permissions and run setup on the new machine. Insert the
tape. A finalized tape needs no original catalog:

```sh
rem tape restore --list
rem tape restore --path Documents/notes.txt --out recovered
```

Use the exact member path from the listing. Repeat `--path` for more files, or
omit it to restore everything. Add `--private-key recovery.remp` for encrypted
objects, including listing encrypted members. If the terminal index is absent
or unusable, walk from BOT:

```sh
rem tape restore --scan --list
rem tape restore --scan --path Documents/notes.txt --out recovered-scan
```

Restores use `candidate-<n>/` subdirectories, with identity in `manifest.json`
and the report. A folder put stores one object per file, so each file lands in
its own candidate directory. Several versions of one path produce several
candidates. Inspect each `manifest.json` and choose the candidates you need;
`--path` limits restoration to named paths but can still return several versions.
Modification times are restored after validation.

To merge selected candidates into `merged/`, use Python 3. This example selects
all candidates in `recovered/` and refuses duplicate paths rather than choosing
a version. If duplicates exist, change `candidates` to the directories you want
and use a fresh merge destination:

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

`tape adopt` imports identity only. It does not import object rows and
cannot make `rem list` or get-by-path work on a new machine. Use catalogless
restore to find and recover files:

```sh
rem tape adopt --label HOME01 --pool default
```

## What to keep besides the tape

> A finalized plaintext tape needs nothing from the original machine to restore
> its files. You still need compatible hardware and a reader. Keep the state
> directory to make listing, normal restore and interrupted-write recovery
> easier; restore the whole directory to the same path. Keep the matching
> `.remp` private files separately for encrypted files, including the recovery
> key in a separate safe place.
> Never restore an older state directory over a newer one without reading the
> `TailBeyondKnownState` refusal and its recovery instructions above. Keep source
> copies while the formats remain under review; see [status](status.md).

## Encryption

Plaintext is the default. Encrypted writes require two to eight distinct
recipient keys, so loss of one private key need not destroy access. One matching
private key is enough to restore. Keep a working key and a recovery key in
separate safe places.

Create a working key locally:

```sh
rem archive recipient keygen --out primary.remp --label primary
```

This creates `primary.remp` (the private file, mode `0600`) and `primary.remr`
(the public file, slot 0). Both paths must be new. `--public FILE.remr` chooses
a different public path. Labels contain at most 32 printable ASCII bytes.
Keep the private file in a safe place and back it up separately; a lost private
file cannot be recovered. Keep the public file on the machine that writes tapes.
It can be copied without exposing the private key. SSH and age keys are not
substitutes for these [key-file formats](reference-cli.md#recipient-key-file-formats).

Create the recovery key, then derive its public record for slot 1. Keygen uses
slot 0; writers need distinct epochs in ascending slot order. With Python 3:

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

Keygen also writes a slot-0 `recovery.remr`; the derive and Python steps
replace it with the slot-1 record.

Keep `recovery.remp` in a separate safe place from `primary.remp` and the tape.

With those two public records:

```sh
rem put private-file --recipient primary.remr --recipient recovery.remr
```

A plaintext put refuses if the newest version is encrypted. Supply recipients
again, or use `--allow-plaintext-copy` only if you intend to store plaintext.

## Ownership refusals

Discovery skips disks and other non-tape devices using their sysfs type.
Do not join `disk` to fix tape discovery. A failed tape/changer probe or an
unresolved changer bay can leave ownership unknown. Fix that probe, or pin a
known standalone serial in the selected config:

```toml
[[standalone_drives]]
serial = "HU1234ABC5"
```

A pin cannot override a changer claim or an unresolved bay.
`--acknowledge-unknown-ownership` allows reads only. A drive without a readable
serial also needs `--allow-unserialized-device` for reads and cannot be written.
See [troubleshooting](guide-troubleshooting.md) for refusal messages.
