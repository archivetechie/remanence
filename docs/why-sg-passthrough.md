# Why Remanence speaks SCSI directly

Remanence never opens `/dev/st*`. Every command a drive or a library
robot receives — read, write, seek, move a cartridge, report status —
is a SCSI CDB built by our own code (`remanence-scsi`) and delivered
through the Linux `SG_IO` ioctl on the raw `/dev/sg*` pass-through
devices. This page explains why, for readers who build their own tape
tooling or want to audit the choice.

The short version: the conventional Linux tape stack splits one physical
system across two tools with private state and no shared vocabulary, and
both properties caused us real operational pain in the archive software
we ran before Remanence. The two cases below are the argument.

## The conventional stack

| Layer | Speaks to | Names things by | Remarks |
| --- | --- | --- | --- |
| `mt` + the kernel `st` driver | drives | device files (`/dev/nst0`) | A translator with private, resettable state between you and the drive. |
| `mtx` | the changer | element addresses (`0x0400`) | A separate namespace; nothing connects an element address to a device file. |
| `SG_IO` pass-through | any SCSI device | serial numbers read from the device | You build every CDB and parse every response yourself. |

The first two layers each work on their own. The seams between them —
and between them and the operator — are where the trouble lives.

## Case 1: kernel state you do not own

In the archive software we operated before Remanence, a reboot of the
tape server produced a reliable failure: every attempt to seek on a tape
returned an I/O error until someone ran `stinit`. The cause sat inside
the `st` driver. When an application asks `st` to seek (the `MTSEEK`
ioctl), the driver translates that into a SCSI LOCATE command, and a
driver option named `scsi2logical` decides how the requested position is
interpreted — as a standard SCSI logical block address, or in an older
device-specific scheme. Our software stored logical block addresses, so
the option had to be 1. But the option lives in kernel-module memory: a
reboot reloads the module with the default of 0, the driver starts
issuing seeks in the wrong addressing scheme, and the drive rejects
them. `stinit` exists to re-apply `/etc/stinit.def` after every boot,
and forgetting it is an error you discover only when a seek fails.

The general lesson is worth stating plainly: under `st`, the correctness
of every positioning command depends on invisible kernel state that the
application does not own, cannot verify, and that silently reverts.

Remanence removes the state rather than managing it:

- Seeks are LOCATE(16) CDBs constructed byte-by-byte in
  `remanence-scsi/src/locate.rs` — the logical block address goes in the
  bytes the SCSI standard assigns it. The addressing scheme is fixed in
  source code. There is no option that can revert, because there is no
  option.
- Every LOCATE is followed by an inline READ POSITION, so the caller
  learns where the head actually settled rather than trusting the seek.
- Drive configuration that `st` would hold on our behalf — fixed block
  size, compression — is issued by Remanence itself, per session, as a
  MODE SELECT (`tape_io::write_config`).
- Resets are detected, not survived by accident. A drive that has been
  power-cycled or reset raises a UNIT ATTENTION condition on the next
  command; Remanence responds by marking the head position unknown and
  flagging the block-size configuration as needing re-verification, and
  it refuses to issue any read or write until both are re-established.
  Where the `st` stack fails as "state silently wrong, I/O error
  mid-operation", this stack fails closed with an explicit error and
  then repairs itself.

After a full server reboot there is no ritual at all: the daemon
restarts and rebuilds everything it needs at mount time, because nothing
it depends on was ever kept in the kernel.

## Case 2: two tools, one library, no shared names

The second failure is a coordination gap rather than a state bug. `mtx`
reports the library's view: cartridges by barcode, drives as "data
transfer element 0, 1, 2…". `mt` operates the host's view:
`/dev/nst0`, `/dev/nst1`, `/dev/nst2`. Nothing in either tool tells you
which element is which device file, and the numbering of one bears no
necessary relation to the numbering of the other.

For most operations you can remain ignorant of the join. For any
operation where writing to the wrong tape is unacceptable — we hit it
when initializing new cartridges — you cannot. Our procedure in the
earlier software was to unload every cartridge from every drive so that
exactly one tape remained mountable, initialize it, and repeat. With
five drives this is as tedious as it sounds. The same folklore exists
around other mt/mtx-based systems: the Bacula and Bareos manuals
describe determining drive indices by loading a tape into one element
and polling each device file to see which one became ready.

The tape industry solved this decades ago under the name *device
serialization*, and the fix is in the SCSI standard: the READ ELEMENT
STATUS command accepts a DVCID bit that asks the library itself to
report the serial number of the drive installed in each bay, and every
drive reports the same serial on INQUIRY VPD page 0x80 through its
device file. Matching serials joins the two namespaces. `mtx` predates
wide support for this and never grew the join; enterprise products all
perform it during their device-discovery phases.

Remanence performs it natively (`remanence-library/src/discovery.rs`):

- Walk every `/dev/sg*` device and classify it with a standard INQUIRY —
  changer or drive.
- Read each drive's serial from VPD page 0x80.
- Issue READ ELEMENT STATUS with DVCID to each changer, obtaining the
  serial of the drive in each bay.
- Join by serial: element address ↔ serial ↔ `/dev/sg*` path.

Discovery is read-only — a test asserts that no state-changing CDB is
ever issued during it — so the mapping for every drive at once costs a
few status reads and moves no tapes. When a library's DVCID response is
partial there is a gap-fill pass; when it is absent there is a
constrained fallback over the host's SCSI topology; and when neither can
resolve the join safely, discovery reports an explicit error. It never
guesses, because a guessed mapping is precisely a write to the wrong
tape waiting to happen. What happens at mount time on top of this join —
the barcode check against the catalog and the beginning-of-tape UUID
check against the medium itself — is covered in the
[tape identity lifecycle explainer](tape-identity-lifecycle-explainer.md).

## What owning the conversation costs, and what it buys

None of this is free. Bypassing `st` means writing and testing
everything it provided: CDB construction, sense-data parsing, timeout
classes per command family, retry and unit-attention handling. That work
is concentrated in one crate (`remanence-scsi`) with fixture transports
that replay canned device responses, so it is tested without hardware —
but it is real work, and anyone considering the same route should weigh
it.

What it buys, in exchange:

- **No state you do not own.** Every parameter that affects correctness
  is either fixed in source or established per session and re-verified
  after resets. There is nothing to re-apply after a reboot and no
  equivalent of `stinit`.
- **One namespace.** Drives and changer are driven through the same
  transport and keyed by serial number, so the mt/mtx join problem
  cannot arise.
- **Errors with their meaning intact.** Failures arrive as SCSI sense
  data and are classified as such, rather than surfacing as an `EIO`
  stripped of everything the drive actually said. Remanence classifies
  sense in both fixed format (`0x70`/`0x71`) and descriptor format
  (`0x72`/`0x73`), so a host or drive configured either way still yields
  correctly-classified end-of-medium and filemark signals.

For where the transport sits in the crate stack, see the
[architecture overview](architecture-overview.md).

<!-- code-anchor: crates/remanence-scsi/src/locate.rs crates/remanence-library/src/handle/tape_io/mod.rs crates/remanence-library/src/discovery.rs @ 244bc6de -->
