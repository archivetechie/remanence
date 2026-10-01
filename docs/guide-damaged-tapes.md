# Damaged tapes

This page is for the day a tape does not behave as expected. It describes what
has usually happened to the tape, what the tools print, what that means, and
what to do next, one situation at a time. Each situation is named for what you
see, because you seldom know the cause when you start. The specification rules
behind it are in REM-PARITY Sections 8.4, 8.5 and 12.6 of the preparing copy,
[REM-PARITY 1.0.0-draft.5](../specs/in-progress/rem-parity-1-specification.md).
The tape's layout is described in the [tape layout reference](reference-tape-layout.md),
and the commands in the [CLI reference](reference-cli.md).

The lines quoted below are the lines the tools print, taken from the code that
prints them. Where a line has fields that depend on the tape, the field is shown
in capitals or as `N`. The specification gives each refusal a name, such as
`BootstrapParse`. That name is the specification's category for the case. The
tools do not print it, so this page quotes the sentence you will see and gives
the category beside it.

## Where to begin

Choose by what you already know.

1. You know the tape's UUID and want to know whether the tape is good. Run
   `rem tape verify-index --tape-uuid TAPE_UUID`. It reads the whole tape and
   reports each failed block. Read [Full verification](guide-full-verification.md),
   and go to "A full verification reports a problem" below.
2. The tape will not identify itself, or an inventory of it takes far longer
   than you expected. Run `rem tape inventory --tape-uuid TAPE_UUID`, and go to
   "The terminal index is damaged, or is not where it was expected".
3. The first record of the tape, the bootstrap, is unreadable, or you suspect
   its values are wrong. Run `rem-debug tape recovery-report SOURCE` with the
   three values you know (the tape UUID, the block size, and the parity scheme),
   and go to "The bootstrap is unreadable" and the situations after it.

A tape can also have more than one of these problems. Start with the one whose
command you can run.

<!-- code-anchor: crates/remanence-cli/src/tape_inventory.rs crates/remanence-api/src/write_owner/terminal_inventory.rs @ c80a553a -->
## A full verification reports a problem

What has happened. One or more data blocks or parity shards that a sidecar
protects failed to read or failed their checksum, or a sidecar or ParityMap
copy or footer has a finding, or the bootstrap's trailing fill is not zero, or
the pass could not be run at all.

What you see. From `rem tape verify-index`:

```text
verification_state: verified_degraded
verified: true
complete: false
verification_basis: measured_full_physical
protected_content_finding: KIND [ADDRESS] DETAIL
```

with one `protected_content_finding` line for each finding. KIND is one of six
names, printed in full: `PROTECTED_CONTENT_FINDING_KIND_DATA_BLOCK`,
`..._PARITY_SHARD`, `..._SIDECAR`, `..._PARITY_MAP`, `..._PREFIX_DAMAGE` and
`..._NOT_PERFORMED`. The address prints as `[]` for the last two. With
`--json`, the same findings are in the `protected_content_findings` array.
[Full verification](guide-full-verification.md) describes each kind, its
address and its details.

What it means. The tape is not complete, and each finding says where the damage
is. `NOT_PERFORMED` means the pass over the protected content could not run, so
the tape is not known to be good. It does not mean that the tape is bad.

What to do next. Take the addresses to whoever will decide whether the Objects
on the tape must be read from another copy. The finding locates the damage; it
does not repair it, and it does not mean that the tape is lost. None of the
commands in the CLI reference acts on a finding. A parity tape can reconstruct
a failed block from its stripe's peers when the stripe has lost no more than `m`
blocks and shards, and it cannot when it has lost more (REM-PARITY Sections
13.4 and 13.5). This documentation does not name an operator command that
performs that read. An epoch whose sidecar metadata is unavailable cannot be
checked against a checksum, and the other epochs are unaffected; the
verification page explains this.

## The terminal index is damaged, or is not where it was expected

What has happened. The terminal index is the inventory of the tape, written at
the end as three replicas, A, B and C, with a separation extent between each
pair. A reader finds it by going to the end of data and reading back. Two
things stop that from working. Either no replica's footer supplies a layout that
a reader can use, or every replica that the layout plans is invalid. A layout is
also not used when the tape's end of data lies beyond the end the layout
planned, which is what happens when something was written after the terminal
suffix: a tape file, a second suffix, or any other structural artifact after C.
The exception is a later suffix that supplies a layout of its own whose
replicas validate; the reader then uses that layout and does not walk
(REM-PARITY Sections 8.4 and 12.6).

What you see. From `rem tape inventory`, a notice before any read from the
beginning of the tape:

```text
bot_recovery: starting full BOT structural walk (block_size=BYTES, reason=no_usable_terminal_layout)
```

The reason is `no_usable_terminal_layout` when no footer supplied a layout the
reader could trust, and `all_members_invalid` when a layout was found and A, B
and C all failed validation. Progress follows, one line for each tape file
crossed:

```text
bot_recovery_progress: tape_file=N position=P:LBA candidates=N elapsed_ms=N
```

and then the tape files that the walk found, with an Object's state in
`bot_object:` lines. `rem-debug tape recovery-report` prints its own report and
not these lines.

What it means. The tape is being read from the beginning, because the index
cannot be trusted where it was planned. A tape that has three good replicas can
be walked for this reason, and that surprises people. If something was written
after C, the layout planned by A, B and C ends before the tape does, so it is
not used, and the tape is walked even though the replicas are intact, unless a
later suffix supplies a layout of its own whose replicas validate. The walk
reports intact replicas as tape files of the walk and not as the authority for
which Objects the tape holds. A trailing artifact appears as an Object
candidate of unknown identity or, when it is torn, as an incomplete candidate
(REM-PARITY Sections 8.4.1 and 12.6). Identities come back as unknown unless
separate host records, such as a surviving checkpoint journal, supply them.

What to do next. Let the walk finish; the [implementation guide](../specs/publication/rem-implementation-guide.md)
says that it can take hours. The need for a walk does not by itself mean that
anything on the tape is lost.

<!-- code-anchor: crates/remanence-parity/src/terminal_inventory.rs crates/remanence-api/src/write_owner/terminal_inventory.rs crates/remanence-cli/src/lib.rs @ c80a553a -->
## Two replicas disagree

What has happened. Two replicas of the terminal index are both valid on their
own and carry different editions of the inventory.

What you see. `rem tape inventory` fails with a data-loss status from the
daemon and prints this line, in which N is the number of conflicting editions:

```text
error: daemon returned data_loss: read terminal tape inventory: terminal inventory found N independently valid conflicting replica editions
```

The command returns no inventory and does not walk. The specification calls
this case `TerminalIndexReplicaConflict`.

What it means. The tape's index is in dispute, and the tools do not choose a
side. REM-PARITY Section 8.5 forbids resolving it by preferring the newer
replica. It also does not require a walk after a conflict, and it names no
operator action.

What to do next. The decision belongs to whoever holds the host records for
the tape. This page gives no procedure for it, because the specification names
none and the tools take no further action. The inventory does not modify the
tape.

<!-- code-anchor: crates/remanence-cli/src/recovery_report.rs crates/remanence-parity/src/error.rs crates/remanence-parity/src/scan.rs crates/remanence-parity/src/bootstrap.rs @ c80a553a -->
## The bootstrap is unreadable

What has happened. The first record of the tape could not be read as a
bootstrap: a medium error, or a filemark or end of data where the record
should be, or a missing magic, or a header checksum that fails, or a payload
whose length runs past the block, or a payload checksum that fails, or valid
checksums and a payload that breaks a later rule of Section 8. In the last
case the tool treats the bootstrap as unreadable unless a value that can still
be decoded disagrees with the values you supplied, and the refusals below cover
that exception.

What you see. With the three values supplied, the scan continues on them, and
the report records that they were used. The human report shows the values and
the fact:

```text
supplied: tape uuid UUID block size BYTES bytes scheme SCHEME
bootstrap treated as unreadable during scan; identity and geometry supplied by hints
```

Without them, discovery ends, because with no tape UUID the role magics that
mark each kind of tape file cannot be derived (REM-PARITY Sections 5.2 and
8.4). `rem-debug tape recovery-report` prints:

```text
error: catalog-less recovery report: discover bootstrap: bootstrap not found anywhere on tape; unreadable bootstrap requires --tape-uuid, --block-size and --scheme
```

The specification calls this outcome `NoBootstrapFound`. The same line is
printed when the first record cannot be parsed as a bootstrap and you
supplied no values, which the specification also reports as `NoBootstrapFound`
at this level of discovery.

What it means. An unreadable bootstrap is not a lost tape. The tape UUID, the
block size and the parity scheme are the identity and geometry the tools need
from it, and you can supply them.

What to do next. Supply all three to `rem-debug tape recovery-report`, which
takes them together or not at all:

```text
rem-debug tape recovery-report SOURCE --tape-uuid UUID --block-size BYTES --scheme k,m,S
```

For a tape written without parity, give `--scheme none`. Without the values,
nothing can be read from this tape by this route.

## The tool refuses the bootstrap or the hints

Sometimes the bootstrap can be read and the tool refuses it. The refusal ends
discovery and no inventory is returned. Which refusal you see says whose value
is wrong. `rem-debug tape recovery-report` prints it as one line:

```text
error: catalog-less recovery report: discover bootstrap: bootstrap refused: DETAIL
```

The cause is DETAIL, and the specification calls every refusal in this section
`BootstrapParse`, except the last, which it names separately.

The supplied block size is wrong. The first record's measured length differs
from the block size you supplied. DETAIL is one of:

```text
short fixed-block bootstrap read: got N bytes, supplied block size is M
bootstrap block larger than supplied block size: got N bytes, expected M
readable bootstrap block size differs from supplied hints: got N bytes, expected M
```

This is the likeliest mistake with hints. Correct `--block-size` and run again.

A supplied value disagrees with a sound header. The header checksum is valid,
and the tape UUID, the block size or the no-parity flag disagrees with your
values, or a parity scheme that can be decoded disagrees with the scheme you
supplied. DETAIL is one of:

```text
tape identity mismatch: readable bootstrap header differs from supplied hints
readable bootstrap block size differs from supplied hints
readable bootstrap parity scheme differs from supplied hints: no-parity flag contradicts scheme
readable bootstrap parity scheme differs from supplied hints
```

The bootstrap is sound and one of your values is wrong. Correct it and run
again.

The bootstrap itself is suspect. The header checksum is valid and a field is
impossible: a format major other than 2, or a sequence other than 0. Or the
bootstrap says the tape is written without parity and its payload nevertheless
carries a scheme record that can be decoded. DETAIL is one of:

```text
unsupported bootstrap schema major version: got N, accept 2
schema-major 2 permits only the sequence-0 BOT Bootstrap: got sequence N
readable bootstrap parity scheme differs from supplied hints
```

Changing your values does not help, because the refusal comes from the
bootstrap and not from them. The tape cannot be read by this route. A valid
header whose payload is damaged is not this case: with values supplied it is
treated as unreadable, and discovery continues, unless the header disagrees
with your values.

Compression was recorded. A parity tape whose bootstrap records that drive
compression was on is refused, and the line is:

```text
error: catalog-less recovery report: discover bootstrap: tape's bootstrap records drive compression; a parity tape must not record drive compression
```

The specification calls this `DriveCompressionEnabled`, and it is a different
case from the others in this section. The message describes the bootstrap that was
recorded on the tape. No drive setting and no value you supply changes it. The format forbids compression on a
parity tape, and the tape was not written as the format requires.

A bootstrap that is treated as unreadable is not trusted for any value. A
bootstrap with an imperfect label that agrees with your values therefore leaves
the tape recoverable, and a bootstrap that contradicts them is refused
(REM-PARITY Section 8.4, and the names in Section 15).

## A tape written without parity

What has happened. The tape was written with parity off. Its bootstrap carries
the no-parity flag and its payload has no parity scheme.

What you see. With a bootstrap that reads, no values are needed and the tape
reads as a no-parity tape. If the bootstrap is unreadable, supply
`--scheme none` together with the tape UUID and the block size. If you supply a
parity scheme for a tape whose header says it has no parity, the tool refuses
with the detail `readable bootstrap parity scheme differs from supplied hints:
no-parity flag contradicts scheme`, which the specification calls
`BootstrapParse`, because the no-parity flag disagrees. A no-parity bootstrap
that carries a scheme record that can be decoded is refused in the same way
when you supply `--scheme none`; when the record cannot be decoded, the
bootstrap is treated as unreadable (REM-PARITY Section 8.2).

What it means and what to do. Supply `--scheme none` only when the bootstrap
cannot be read. A tape written without parity has the same terminal suffix as
a parity tape and has no parity closeout, so it holds more user data than a
parity tape of the same size (see [Object sizing](guide-object-sizing.md)). It
also has no sidecars, so a full verification of it has no protected blocks to
read.

## In plain terms

A tape carries two things that let a stranger read it: a label at the
beginning, the bootstrap, and a contents list at the end, the terminal index,
written three times. Damage to either does not make the tape unreadable,
because the tools can start from the values you know and can read the whole
tape from the beginning when the contents list cannot be found. What the tools
will not do is guess. When the label and what you tell them disagree, or when
two copies of the contents list disagree, they stop and say so, because a wrong
guess about a tape is worse than a slow answer. A full verification is the
different question, whether every page can be read, and the
[Full verification](guide-full-verification.md) page answers it.
