# Why Remanence speaks SCSI directly

This page is for someone who builds tape tooling, or who wants to audit how Remanence talks
to hardware. Remanence never opens the kernel tape devices (`/dev/st*`, `/dev/nst*`). Every
command that a drive or a library robot receives, whether to read, write, seek, move a
cartridge or report status, is a SCSI command descriptor block (CDB, the byte string that
carries one SCSI command). Remanence builds each CDB in its own code, in the `remanence-scsi`
crate, and sends it through the Linux `SG_IO` ioctl on the SCSI generic devices, `/dev/sg*`
(see [SG_IO and sg device](reference-glossary.md#sg_io-and-sg-device)). This page explains
why.

The usual Linux tape tools split one physical system between two programs that keep private
state and share no names for the hardware. Both properties caused operational problems in the
archive software we ran before Remanence. The two cases below describe those problems and
what Remanence does instead.

## The conventional stack

| Layer | Speaks to | Names things by | Remarks |
| --- | --- | --- | --- |
| `mt` and the kernel `st` driver | drives | device files (`/dev/nst0`) | The driver translates requests into SCSI commands and keeps settings of its own between the program and the drive. |
| `mtx` | the changer | element addresses (`0x0400`) | A separate set of names. Nothing in it connects an element address to a device file. |
| `SG_IO` pass-through | any SCSI device | serial numbers read from the devices | The program builds every CDB and parses every response itself. |

An element address is the changer's number for a location such as a slot or a drive bay (see
[element address](reference-glossary.md#element-address)). Each of the first two layers works
on its own. The problems appear where they meet each other, and where they meet the operator.

## Case 1: kernel state you do not own

In the archive software we ran before Remanence, a reboot of the tape server caused a
reliable failure. Every seek on a tape returned an I/O error until someone ran `stinit`.

The cause was inside the `st` driver. A program that asks `st` to seek uses the `MTSEEK`
ioctl, and the driver turns that into a SCSI LOCATE command, which moves the tape to a given
block. A driver option, `scsi2logical`, decides how the requested position is read: as a
standard SCSI logical block number, or as a device-dependent address. Its default is off, so
the driver uses device-dependent addresses unless the option is set
([Linux SCSI tape driver documentation](https://docs.kernel.org/scsi/st.html), the
`MT_ST_SCSI2LOGICAL` option). Our software stored logical block numbers, so the option had
to be set. The option lives in the driver's memory, though, and a reboot reloads the driver
with its default. The driver then issued seeks in the wrong form, and the drive rejected
them. `stinit` exists to apply the settings in `/etc/stinit.def` after the driver is loaded
([stinit(8)](https://github.com/iustin/mt-st/blob/master/stinit.8)), and a forgotten
`stinit` shows up only when a seek fails.

Under `st`, then, the correctness of every positioning command depends on settings that the
program does not own and cannot easily check, and that return to their defaults when the
driver reloads.

Remanence has no such settings to manage. Its seeks are LOCATE(16) commands, built byte by
byte in `remanence-scsi/src/locate.rs`, with the logical block address in the bytes the SCSI
standard assigns to it. The addressing mode is fixed in the source code, so there is no
driver option to lose. Every LOCATE the drive handle issues is followed at once by READ
POSITION, the command that reports where the tape is, so the caller learns where the head
actually stopped instead of trusting the seek.

The drive settings that `st` would hold on a program's behalf, the fixed block size and
hardware compression, are set by Remanence itself with MODE SELECT, the command that changes
a drive's mode parameters (`tape_io::write_config`). It sends them as volatile settings, which
last until the drive is reset. Before a write it sends them again and reads them back with
MODE SENSE, the command that reports a drive's mode parameters, and it writes nothing unless
the drive reports the fixed block size it asked for with compression off.

Resets are detected. A drive that has been power-cycled or reset reports a UNIT ATTENTION
condition on the next command, a SCSI status that tells the host the drive's state has
changed (see [unit attention](reference-glossary.md#unit-attention-ua)). Remanence then marks
the head position as unknown and marks the block-size setting as needing to be checked again.
It refuses every read and write, with an explicit error, until the position has been
re-established and MODE SENSE has confirmed the block size. A write session does both before it starts. Under `st`, the same
reset typically surfaces as a wrong setting and an I/O error in the middle of an operation.

After a full server reboot there is nothing to apply again by hand. The daemon restarts and
sets the drive modes it needs itself, because nothing it depends on is held in the kernel.

## Case 2: two tools, one library, no shared names

The second problem is a gap between the two tools rather than a fault in either. `mtx`
reports the library's view: cartridges by barcode, and drives as data transfer elements 0, 1,
2 and so on. `mt` works on the host's view: `/dev/nst0`, `/dev/nst1`, `/dev/nst2`. Nothing in
either tool's output says which element is which device file, and the numbering of one has
no necessary relation to the numbering of the other.

Most operations can do without that link. An operation for which writing to the wrong tape is
unacceptable cannot, and we met this when initializing new cartridges. The procedure in the
earlier software was to unload every cartridge from every drive, so that exactly one tape
remained mountable, initialize that tape, and repeat. With five drives this was slow.

SCSI itself provides the link. The changer's inventory command, READ ELEMENT STATUS, has a bit
named DVCID that asks the library to report, for each drive bay, the identity of the drive
installed in it, including its serial number. The bit is defined in the T10 Media Changer
Commands standard ([SMC-3](https://www.t10.org/drafts.htm)), and HPE documents it for its
libraries in the HPE Storage Tape Library SCSI Reference Guide (part number
20-STG-TAPESCSIREF-ED5, "Read Element Status (B8h)"). Every drive reports the same serial
number through its own device, in the Unit Serial Number page (vital product data page 0x80)
that the INQUIRY command returns (IBM LTO SCSI Reference, GA32-0928-08, section 6.3.3).
Matching the two serial numbers connects the two sets of names.

Remanence performs this match during discovery, in `remanence-library/src/discovery.rs`:

1. It walks the `/dev/sg*` devices and classifies each one with a standard INQUIRY, as a
   changer or a tape drive. Other devices are skipped.
2. It reads each drive's serial number from VPD page 0x80.
3. It sends READ ELEMENT STATUS with DVCID set to each changer, and reads the serial number of
   the drive in each bay.
4. It joins the two by serial number: element address, serial number and `/dev/sg*` path.

Discovery is read-only. A test records every CDB that discovery sends and fails if any of
them could change the state of a device, so mapping every drive at once costs a few status
reads and moves no tapes. Some library firmware reports drive identities only partly. When
the first READ ELEMENT STATUS leaves a bay without a serial number, discovery asks again for
the drive bays alone, in two variants, and fills in the serial numbers that the later answers
supply. A bay that is still unresolved after that is reported with the warning
`DriveMappingUnavailable`, and the daemon refuses state-changing operations on it. Discovery
does not guess a mapping, because a guessed mapping can lead to a write on the wrong tape.
`rem libraries` prints the result (see the [CLI reference](reference-cli.md#rem-libraries)).

On top of this join, every load checks the cartridge itself: the barcode against the
catalog, and the tape UUID in the bootstrap against the medium. The
[tape identity explainer](tape-identity-lifecycle-explainer.md) describes those checks.

## What speaking SCSI directly costs, and what it provides

Bypassing `st` means writing and testing everything it provided: building CDBs, parsing sense
data (the drive's detailed report of an error or condition), choosing a timeout for each kind
of command, and handling retries and unit attentions. That work is concentrated in one crate,
`remanence-scsi`, and the layer above it is tested with fixture transports that replay
recorded device responses, so most of it is tested without hardware. It is still a large
amount of work, and anyone considering the same route should weigh it.

In exchange, Remanence holds no state that it does not own. Every setting that affects
correctness is either fixed in the source code or set when a drive is prepared, and it is
checked again after a reset. There is nothing to apply after a reboot, and no equivalent of
`stinit`.

Drives and changers are reached through the same transport and are named by serial number,
so the problem of matching `mt` devices to `mtx` elements does not arise.

Errors keep their meaning. Failures arrive as SCSI sense data and are classified as such,
instead of reaching the program as a bare `EIO` with everything the drive said removed.
Remanence decodes sense data in both of the formats SCSI defines, fixed format (response
codes `0x70` and `0x71`) and descriptor format (`0x72` and `0x73`), so a filemark or the end
of the medium is recognised whichever format the drive uses.

## In plain terms

The usual Linux tools depend on settings and numbering kept inside the kernel, which can be
lost at a reboot or disagree with each other. Remanence talks to the hardware in its own
code. It keeps its own records, such as the drives it has seen, in its catalog, and it rebuilds
the current mapping of drives from the hardware each time it discovers them. Nothing it relies
on is hidden in `st` driver configuration. For where the transport sits among the crates, see
the [architecture overview](architecture-overview.md).

<!-- code-anchor: crates/remanence-scsi/src/locate.rs crates/remanence-scsi/src/sense.rs crates/remanence-scsi/src/mode.rs crates/remanence-library/src/handle/tape_io/mod.rs crates/remanence-library/src/discovery.rs crates/remanence-library/src/transport.rs @ 311cc6c3 -->
