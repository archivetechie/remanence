<!-- code-anchor: crates/remanence-format-driver/src/lib.rs crates/remanence-cli/src/lib.rs crates/remanence-daemon/src/entry.rs crates/remanence-api/src/catalog_conversion.rs crates/remanence-state/src/index.rs @ 8238b7b3 -->
# Foreign-format adapters

This page is for someone building a Remanence distribution that reads an
archive format other than Remanence's own. A foreign format is any archive
format other than REM-OBJECT, for example the format an earlier archive system
wrote to its tapes. An adapter is a parser for one foreign format: it
recognizes an archive in that format and reads its files out, so that the same
`rem archive` scan, restore and recovery commands work on it. An adapter only
reads. Nothing in this interface writes a new archive in a foreign format.

A dump is a file holding the bytes of one foreign-format archive, for example
an image copied off a tape. An adapter can also read the records of a tape
loaded in a drive directly.

## Stock Remanence has no adapters

The core repository defines the adapter interface and the registry that holds
adapters, in the crate `remanence-format-driver`. It contains no adapter. The
stock `rem`, `rem-debug` and `rem-daemon` binaries start with an empty
registry. To list the foreign formats a binary was built with:

```sh
rem archive formats
```

The stock binary prints an empty list:

```text
{"foreign_formats":[],"observed_endpoints":[],"supported_endpoint_kinds":["library","standalone"]}
```

The archive commands accept any non-empty value for `--format <ID>`, and the
stock binaries refuse every value as unregistered. For example,
`rem archive probe --format legacy-example --dump <PATH>` exits with status 1
and prints:

```text
error: unsupported format operation: foreign format legacy-example is not registered in this distribution
```

Concrete adapters are not core crates or optional core features. A separate
distribution chooses its adapter crates at build time, registers them, and
starts the same command-line and daemon code with that registry. Each
adapter's maintenance, release schedule and support therefore stay separate
from Remanence's own format. The public
[`remanence-adaptors`](https://github.com/archivetechie/remanence-adaptors)
repository is one such distribution. It is versioned independently and is not
part of a Remanence release.

## Building a distribution

A distribution creates a `ForeignFormatRegistry`, registers each adapter, and
passes the registry to the entry points that the stock binaries call with an
empty one:

| Program | Entry point |
| --- | --- |
| `rem` | `remanence_cli::main_entry_with_registry(registry)` |
| `rem-debug` | `remanence_cli::debug_main_entry_with_registry(registry)` |
| `rem-daemon` | `remanence_daemon::main_entry_with_registry(registry)` |

A minimal `rem` for a distribution with one adapter, `LegacyFormat`, from a
crate of the distribution's own:

```rust
use std::process::ExitCode;
use std::sync::Arc;

use remanence_format_driver::ForeignFormatRegistry;

fn main() -> ExitCode {
    let mut registry = ForeignFormatRegistry::new();
    if let Err(error) = registry.register(Arc::new(legacy_format::LegacyFormat)) {
        eprintln!("error: assemble foreign-format registry: {error}");
        return ExitCode::from(1);
    }
    remanence_cli::main_entry_with_registry(registry)
}
```

Registration refuses anything that would make the choice of adapter
ambiguous:

| Error | Message | Cause |
| --- | --- | --- |
| `InvalidId` | `invalid foreign-format id "<ID>"` | The format ID is empty or has leading or trailing whitespace. |
| `InvalidAlias` | `invalid foreign-format alias "<ALIAS>"` | The same, for an alias. |
| `DuplicateId` | `foreign-format id "<ID>" is already registered` | Another adapter has the same format ID. |
| `DuplicateName` | `foreign-format name "<NAME>" is already registered` | An ID or alias would name two adapters, or one adapter lists the same name twice. |

Adapters are linked in when the distribution is built. Remanence does not load
plugins from a path at run time.

In a distribution, `rem archive formats` lists each adapter with its ID,
version, aliases and the sources it reads. For an adapter that reads dumps
only, the entry has this shape:

```text
{"foreign_formats":[{"aliases":[],"id":"<ID>","sources":["byte-stream-dump"],"version":"<VERSION>"}],"observed_endpoints":[],"supported_endpoint_kinds":["library","standalone"]}
```

## Commands that use an adapter

| Command | Source | Remarks |
| --- | --- | --- |
| `rem archive probe --format <ID> --dump <PATH>` | dump | Checks whether the dump is in this format, without reading its entries. |
| `rem archive scan --format <ID> --dump <PATH>` | dump | Lists the entries and any damage. |
| `rem archive restore --format <ID> --dump <PATH> --dest <DIR>` | dump | Restores the files into a directory. |
| `rem archive recover --format <ID> --dump <PATH> --dest <DIR>` | dump | Writes what can be read into sparse partial files, for damaged archives. |
| `rem-debug archive probe`, `scan`, `restore`, `recover` | dump, or tape with `--tape <SERIAL> --bay <BAY> [--rewind]` | The same commands; a tape source reads the cartridge loaded in that drive bay of the library with that serial. |

`--format` takes the adapter's ID or one of its aliases. The command then
checks that the adapter reads the kind of source given, and refuses with
`format <ID> does not support the requested source` when it does not. Before
opening a reader, it probes the source and refuses if the probe finds no match
at the current position.

## The adapter interface

An adapter implements the trait `ForeignFormatAdapter`. The trait is object
safe, which means the registry can hold different adapters behind one pointer
type, `Arc<dyn ForeignFormatAdapter>`, and choose among them at run time. An
adapter provides:

- a stable format ID, recorded in catalogs, and a version string for the
  format contract it reads (`id`, `version`);
- any short aliases accepted on the command line (`aliases`);
- the source kinds it reads (`supported_sources`): `ByteStreamDump`, a
  seekable dump, and `PhysicalTapeRecords`, tape records and filemarks read
  from a drive, or both;
- for each source it reads, a probe that does not change the source and an
  opener (`probe_dump` and `open_dump_reader`, `probe_tape` and
  `open_tape_reader`). A dump probe returns the stream to where it found it.
  The defaults for a source the adapter does not read return an "unsupported"
  error.

A probe returns a confidence (`NoMatch`, `Possible`, `Probable` or `Certain`)
and adapter-owned state as opaque bytes, which the caller passes back to the
opener. An adapter that needs to remember a variant, an offset or an index
between the probe and the read keeps it in that state.

The opener returns an `ArchiveReader`, which reads the archive as normalized
events, the same for every format:

- `scan` reports each entry and any damage to a catalog sink without
  restoring payloads, and returns a scan report;
- `stream_all` delivers every entry and its payload bytes in archive order;
- `stream_file` delivers one file by its ID, or returns an "unsupported
  operation" error when the format cannot seek to one file.

An entry has a file ID, a UTF-8 path, a kind (regular file, directory,
symbolic link, hard link, or special file), an optional link target, an
optional size, and adapter-owned state. Damage that belongs to one file is
reported as a byte range of that file, with a status: checksum failed, read
error, missing, or unsupported. Source bytes that belong to no file are
reported as an archive gap, with a cause: unrecognized data, read error,
missing, resynchronization (bytes skipped while looking for the next valid
boundary), or unsupported.

The scan report counts entries, damage events and archive gaps, and states
the integrity basis the scan actually established: none (`Unknown`), a
content hash per entry (`ContentHash`), the foreign format's own checksums
(`FormatChecksum`), or parity consistency (`ParityConsistency`). This is
deliberately separate from the capability flags an adapter advertises: the
report says what this scan did, so an adapter able to verify checksums
reports `Unknown` for a scan that did not check them.

## Foreign archives in the catalog

The catalog has a table of catalog units, the things it can list entries for
(see [catalog unit](reference-glossary.md#catalog-unit)). A unit for a foreign
archive records the adapter's format ID, the source kind and source, scan
counts, and the adapter's state. The daemon's `ListEntriesInUnit` call opens
such a unit's source with the registered adapter of that ID, scans it, and
returns the entries it reports. It reads only units whose source kind is
`byte_stream_dump`, a dump file on the daemon's host. It answers
`UNIMPLEMENTED` when the format is not registered in the distribution, when
the source is a tape, and for units that hold native Remanence objects.

No command or call in stock Remanence records a foreign unit. A distribution
that imports foreign archives records them through `remanence-state`
(`CatalogIndex::upsert_foreign_archive_projection`), with a source identifier
from the daemon's own configuration rather than one a client supplied. Search
and other views across formats belong to the application above Remanence; the
adapter supplies readable entries and the data stream.

## See also

- [`rem archive formats`](reference-cli.md#rem-archive-formats) and the other
  archive commands in the CLI reference.
- [Importing and recovering Remanence tapes](importing-and-recovering-remanence-tapes.md),
  for tapes that Remanence itself wrote.
