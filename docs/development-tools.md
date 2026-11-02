<!-- code-anchor: crates/remanence-cli/Cargo.toml crates/remanence-cli/src/rem_dev.rs crates/remanence-cli/src/dev/generate_tape_images.rs crates/remanence-cli/src/dev/freeze_drill.rs crates/remanence-cli/src/dev/terminal_index_drill.rs Makefile @ 8238b7b3 -->
# Development tools

This page is for contributors. It describes `rem-dev`, the development
program in the `remanence-cli` package. `rem-dev` generates and checks the
review-only REM-PARITY tape images, runs two drills against a scratch tape in
a real or virtual drive, and writes raw dump files to a scratch tape for test
setup. Operators do not need it. The release workflow builds `rem`,
`rem-debug`, `rem-daemon` and `rem-recover` by name, so no release archive
contains `rem-dev`.

## Building rem-dev

`rem-dev` needs the `fixtures` feature, which turns on deliberately
destructive test fixtures in `remanence-parity` and `remanence-state`. No
production binary is built with it, and a plain build skips `rem-dev`. To
build it:

```sh
cargo build --release -p remanence-cli --features fixtures --bin rem-dev
```

The program is `target/release/rem-dev`. Without the feature, Cargo refuses:

```text
error: target `rem-dev` in package `remanence-cli` requires the features: `fixtures`
```

To run its tests:

```sh
cargo test -p remanence-cli --features fixtures --bin rem-dev
```

The Makefile target `make rem-dev` is unrelated to this program despite its
name. It builds the debug `rem` and `rem-debug` and grants `target/debug/rem`
the `cap_sys_rawio` capability with `sudo setcap`, because a rebuild can
replace the file and drop the capability.

## Global options

| Option | Effect |
| --- | --- |
| `--allow <ALLOW>` | A library serial that this invocation may change. Repeatable. Only `dev write-dump-to-tape` consults it. |
| `--allow-derived <ALLOW_DERIVED>` | A library serial whose drive bays may be used even though their identity was derived rather than reported by the drive. Repeatable. Used with `--allow`. |

The two drills do not consult `--allow`: each acts only on the device given
with `--device`.

## generate-tape-images

```text
rem-dev generate-tape-images [--check] [--negative-cases <PATH>] [--negative-supplement <PATH>]
rem-dev generate-tape-images --export-images <DIR>
rem-dev generate-tape-images --export-objects <DIR>
rem-dev generate-tape-images --refresh-resume-inputs
```

The command generates the review-only REM-PARITY tape images, and their
damage, resume and negative vectors, under
`fixtures/rem-parity-terminal-index-draft/tape-images/` in the source tree.
The path is fixed when the program is built, so run it from the checkout it
was built from. Its `--help` lists no options, because the options are read
by the generator itself.

| Option | Effect |
| --- | --- |
| (none) | Writes every artifact that is missing or different. It refuses to change an existing expected outcome, with `existing fixture differs; refusing to change <PATH>`. |
| `--check` | Writes nothing. It regenerates every artifact in memory and fails on the first difference (`fixture differs: <PATH>`) or on a file that should not be there. |
| `--negative-cases <PATH>` | Reads the negative cases from `<PATH>` instead of `negatives/negative-cases.json`. |
| `--negative-supplement <PATH>` | Reads the supplementary negative cases from `<PATH>` instead of `negatives/negative-cases-supplement.json`. |
| `--export-images <DIR>` | Writes each image's tape files to `<DIR>/<IMAGE>/tape-file-<NNN>.bin`; a file that ends without a filemark gets the suffix `-torn`. |
| `--export-objects <DIR>` | Writes each image's object tape files to `<DIR>`. It refuses a directory inside `fixtures/`. |
| `--refresh-resume-inputs` | Rewrites only the inputs of the resume vectors after a contract revision. Existing expected outcomes and tape digests stay unchanged. |

Continuous integration runs the check on every push. To run it:

```sh
cargo run --release -p remanence-cli --features fixtures --bin rem-dev -- generate-tape-images --check
```

It prints the layout of each image and the cases it cannot execute, then a
summary line (shortened):

```text
...
CHECK PASS: all seven image digests, 72 damage descriptors and 13 resume cases; all 7a/7b/7c negative descriptors, 48 supplement variants, 1 erratum E1 negative cases and erratum E2's 41 overrides
```

## tape freeze-drill

```text
rem-dev tape freeze-drill [OPTIONS] --device </dev/sgN> --block-size <BLOCK_SIZE> --damage-plan <DAMAGE_PLAN> --report <PATH.json>
```

The freeze drill destroys the contents of the scratch tape in the drive at
`--device`. It writes two drill objects through the production parity code
and a complete finalization of the terminal index. It then reads the tape
back with read errors injected according to `--damage-plan`, scans it without
a catalog, recovers what the damage plan made unreadable, and verifies each
object by its digest. The injected errors are simulated in the reader; the
medium is not damaged. The drill is the live round trip that
[REM-PARITY freeze criterion 4](../specs/README.md#rem-paritys-freeze-criteria)
asks for, which must pass at two block sizes.

The drill checks the tape at the beginning of tape (BOT) before it writes:

- a blank tape is given a new drill identity;
- a tape whose tape UUID carries the drill's identity marker, from an earlier
  drill, is reused;
- an unreadable BOT is overwritten only with `--yes-i-know-scratch`;
- any other Remanence tape, a tape in a foreign format, and unrecognized data
  are refused. A Remanence tape is refused with
  `refusing to overwrite foreign Remanence tape identity <UUID>; it has no freeze-drill identity marker`.

| Option | Effect |
| --- | --- |
| `--device </dev/sgN>` | The SCSI generic device of the drive holding the scratch tape. The drill's only target. |
| `--block-size <BLOCK_SIZE>` | `262144`, `524288` or `1048576`: fixed records of 256 KiB, 512 KiB or 1 MiB. |
| `--damage-plan <DAMAGE_PLAN>` | `bootstrap-copy0` (the BOT bootstrap copy), `sidecar-head` (one sidecar's primary header and index copy), `object-span` (a span of object data that parity can repair), `terminal-replica-c` (terminal replica C, so that B or A must be used), or `combined` (the largest supported set of these). |
| `--data-mib <DATA_MIB>` | The total payload of the drill objects, in MiB. Default 512; must be greater than 0. |
| `--default-index-separation` | Uses the production separation extent of 1 GiB between terminal replicas instead of the drill's three-record extent. |
| `--yes-i-know-scratch` | Permits overwriting a tape whose BOT cannot be read. |
| `--report <PATH.json>` | Where the JSON report is written. It must not name the device, an existing path must be a regular file, and its directory must exist. |

The report has `report_version` 1. It records the tape UUID, block size,
payload size and separation profile, and whether drive compression was
verified off. It then records the damage plan, the injected and observed fault
positions, the scan, the number of reconstructed blocks, the object
verification, `expectations_met`, `expectation_failures`, the time of each
phase, and `success`. The program
prints one summary line that starts with `freeze-drill passed:` or
`freeze-drill failed:` and names the report.

## tape terminal-index-drill

```text
rem-dev tape terminal-index-drill [OPTIONS] --device </dev/sgN> --tape-uuid <UUID> --block-size <BLOCK_SIZE> --report <PATH.json>
```

The terminal-index drill only reads. It needs a finalized tape in the drive
at `--device`. A healthy pass finds the terminal index from the end of data:
replica A, a separation extent, replica B, a second separation extent, and
replica C. The damage plan then injects read failures in the reader, above
the SCSI transport, and checks that the reader falls back from C to B to A,
or hands over to recovery from BOT when all three fail. It never rewrites the
medium.

| Option | Effect |
| --- | --- |
| `--device </dev/sgN>` | The SCSI generic device of the drive holding the finalized tape. |
| `--tape-uuid <UUID>` | The tape's exact tape UUID. A barcode is not accepted. |
| `--block-size <BLOCK_SIZE>` | The tape's fixed block size, as its catalog records it. |
| `--damage-plan <DAMAGE_PLAN>` | `none` (the default: C must be selected), `a`, `b`, `c`, `ab`, `ac`, `bc` (make those replicas unreadable), `abc` (all three, so recovery from BOT is required), `disagreement` (replace A with a valid but conflicting edition; the drill then requires the `TerminalIndexReplicaConflict` refusal with no replica selected), or `gap-ab-header`, `gap-ab-footer`, `gap-bc-header`, `gap-bc-footer` (replace that record of a separation extent with a block of `0xd7` bytes). |
| `--full-verify` | Also walks the measured physical prefix of the tape and validates all five terminal files. Allowed only with `none` or a gap plan, and required by every gap plan. |
| `--reconcile-outcome <RECONCILE_OUTCOME>` | Forces one reconciliation refusal without writing: `torn-worm` makes the next component readable at its proved start only as a torn tail of a [WORM](reference-glossary.md#worm-and-notworm) (write once, read many) tape, and `unproved-start` makes its start unprovable. Requires `--damage-plan none`. |
| `--report <PATH.json>` | Where the JSON report is written. |

The report has the schema `rem.tape.terminal-index-drill.v1`. It names the
injection mechanism and the exact positions, as logical block addresses
(LBAs), that it made unreadable or replaced. It records the terminal layout,
the fast inventory, any recovery from BOT, the full verification, any
reconciliation, `expectations_met`, `expectation_failures` and `success`.

In the report's `full_verify` object, `outcome` says what the terminal index
did and `complete` says what the whole tape did. An `outcome` of
`"verified_complete"` with `complete` set to `false` means the terminal index
validated and the protected content did not; `complete` is `true` only when
both did. [Full verification](guide-full-verification.md) explains the
same distinction for `rem tape verify-index`.

## dev write-dump-to-tape

```text
rem-dev dev write-dump-to-tape [OPTIONS] --dump <PATH> --tape <SERIAL> --bay <BAY>
```

This command overwrites the tape loaded in a drive bay of a library with the
raw bytes of a dump file, to set up a test fixture. It rewinds, switches the
drive to variable-length records with compression off, writes the file in
records of `--record-size` bytes, writes one filemark and rewinds again. On
success it prints one line starting `ok: wrote`, with the bytes and records
written. The library's serial must be given with `--allow`, and the command
must be confirmed with `--i-understand-this-overwrites-the-loaded-tape`:

```sh
rem-dev --allow <SERIAL> dev write-dump-to-tape --dump <PATH> --tape <SERIAL> \
    --bay <BAY> --i-understand-this-overwrites-the-loaded-tape
```

| Option | Effect |
| --- | --- |
| `--dump <PATH>` | The dump file: a regular, non-empty file. |
| `--tape <SERIAL>` | The serial of the library that holds the tape. |
| `--bay <BAY>` | The drive bay that holds the tape. |
| `--record-size <BYTES>` | The record size. Default `1048576`; binary suffixes such as `1MiB` are accepted. It must not exceed 16 MiB minus one byte, the WRITE(6) transfer limit, or the drive's reported maximum block size. |
| `--i-understand-this-overwrites-the-loaded-tape` | Required. Without it the command refuses before it looks for the library. |

Without the serial in `--allow`, the command refuses with
`library "<SERIAL>" not on the --allow list — state-changing ops are refused`.

## Exit status

`generate-tape-images` exits with 0 on success and 1 on any failure.
`dev write-dump-to-tape` exits with 0 on success, 2 when no library with the
given serial is on this host, and 1 on any other failure. The two drills exit
with 0 when the drill passed, 2 when it ran and wrote its report but an
expectation failed, and 1 when it could not run or could not write its
report.
