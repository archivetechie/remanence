# Foreign-format adapters

Remanence can expose files from an older archive format through the same
normalized scan, restore, and recovery paths used by its own tools. This is a
read-only compatibility boundary: an adapter may identify and read an existing
archive, but it cannot write new archives in that foreign format.

## Core and distribution boundary

The core repository owns `remanence-format-driver`, including normalized
archive events, the object-safe adapter trait, and `ForeignFormatRegistry`.
The stock `rem`, `rem-debug`, and `rem-daemon` binaries construct an empty
registry. They therefore accept the generic `--format <ID>` syntax but reject
every foreign ID as unregistered.

Concrete parsers are not core crates or optional core features. A separate
distribution selects adapter crates at compile time, registers them, and calls
the registry-aware CLI and daemon entrypoints. This keeps parser maintenance,
release cadence, and support status separate from the native Remanence format.

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

An application may import metadata for an older archive into the catalog and
record the adapter's stable format ID and source. When the same adapter is
registered in the daemon distribution, catalog entry listing opens that source
through the generic reader. Search and other cross-format views belong to the
application above this mechanism layer; the adapter supplies the readable
entries and data stream.
