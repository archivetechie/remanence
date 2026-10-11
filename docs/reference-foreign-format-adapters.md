<!-- code-anchor: crates/remanence-format-driver/src/lib.rs crates/remanence-cli/src/lib.rs @ 6ed03ef0 -->
# Foreign-format adapters

A foreign format is an archive format other than REM-OBJECT, for example the
format of an earlier archive system. Remanence can expose the files in such an
archive through the same scan, restore, and recovery commands it uses for its
own objects. An adapter is the parser that makes this possible. It identifies
and reads an existing archive, and it cannot write a new archive in the
foreign format.

A dump is a file that holds the bytes of one foreign-format archive, for
example an image copied off a tape. Adapters can also read the records of a
mounted tape directly.

## Core and distribution boundary

The core repository owns `remanence-format-driver`, including normalized
archive events, the object-safe adapter trait, and `ForeignFormatRegistry`.
The stock `rem`, `rem-debug`, and `rem-daemon` binaries construct an empty
registry. They therefore accept the generic `--format <ID>` syntax but reject
every foreign ID as unregistered.
`rem archive formats` prints the registry linked into the CLI distribution as
JSON; stock Remanence reports an empty `foreign_formats` array.

Concrete parsers are not core crates or optional core features. A separate
distribution selects adapter crates at compile time, registers them, and calls
the registry-aware CLI and daemon entrypoints. Parser maintenance, release
schedule, and support status are therefore separate from the native Remanence
format.

The auxiliary
[`remanence-adaptors`](https://github.com/archivetechie/remanence-adaptors)
repository demonstrates that assembly. It is independently versioned and is
not part of the Remanence core release.

## Adapter contract

Each registered adapter provides:

- one stable format ID for catalogs and any short CLI aliases;
- the source kinds it can read: a seekable byte-stream dump, physical tape
  records, or both;
- a non-destructive probe for each supported source;
- an `ArchiveReader` that emits normalized entries, payload bytes, damage
  ranges, and unattributed source gaps;
- a scan report that states the integrity basis actually established during
  that scan, independently of the adapter's advertised capabilities.

Dump readers receive a seekable source and any adapter-owned state persisted
with an imported catalog unit. An adapter that needs variant, offset, or index
state must encode it for the catalog during import and consume it when the unit
is reopened. Stateless adapters may leave the state empty.

The registry rejects duplicate IDs and aliases so dispatch is deterministic.
Adapters are linked at build time; Remanence does not load native-code plugins
from arbitrary paths at runtime.

## Catalog use

An application may import the metadata of a foreign-format archive into the
catalog as a catalog unit (see the [glossary](reference-glossary.md#catalog-and-daemon)) and
record the adapter's stable format ID and the source. When the same adapter is
registered in the daemon distribution, catalog entry listing opens that source
through the generic reader. Search and other cross-format views belong to the
application above this mechanism layer; the adapter supplies the readable
entries and data stream.
