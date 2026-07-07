# On-tape layout reference

What a Remanence-written cartridge physically contains, as the code writes
it today. Byte-level detail lives in the published specifications —
[RAO 1.0](../specs/rao-1.0-specification.md),
[RAO 1.1](../specs/rao-1.1-specification.md), and
[REM-PARITY 1.0](../specs/rem-parity-1.0-specification.md) — this page is
the orientation layer above them.

The design goal behind all of it: a tape must be readable with no access
to Remanence's host state. Everything the catalog knows is either written
to the tape itself or rebuildable from journals; the SQLite index is a
cache, never the truth.

<!-- code-anchor: crates/remanence-parity/src/filemark_map.rs crates/remanence-parity/src/sink.rs crates/remanence-parity/src/bootstrap.rs @ 7fb10f8 -->
## Tape files and filemarks

A cartridge is a sequence of tape files separated by filemarks, written in
fixed-size blocks. Four kinds of tape file exist (their codes are stable
on tape): `Object` (0), `ParitySidecar` (1), `Bootstrap` (2), and
`ParityMap` (3).

- **Bootstrap** blocks are the tape's self-description. Copy 0 always sits
  at LBA 0 — the beginning of tape — and further copies are spread down
  the tape at roughly 5% capacity intervals. A bootstrap records the tape
  UUID, the fixed block size, the parity scheme, a digest of the filemark
  map, a sequence number, and the writer version. Because the block size
  is in the bootstrap, a reader never needs MODE SENSE state to start; a
  scan just probes the candidate block sizes (256 KiB, 512 KiB, 1 MiB) at
  BOT.
- **Object** tape files contain only body-format blocks (a stored RAO
  object). The parity layer owns every filemark; body formats cannot emit
  them.
- **Parity sidecar** tape files carry the Reed-Solomon parity shards and
  index for the data written since the last sidecar.
- **Parity map** tape files are a directory of sidecar epochs, written
  when the map no longer fits inline in the bootstrap.

The only fixed literal on tape is the bootstrap magic, the 8 bytes
`52 45 4D 00 42 4F 4F 01` (`REM\0BOO\x01`). Sidecar, sidecar-footer, and
parity-map magics are derived per tape as the first 8 bytes of an
HMAC-SHA-256 keyed by the tape UUID, so blocks from one tape cannot
masquerade as another's. All parity-layer structures carry CRC-64/XZ
checksums.

<!-- code-anchor: crates/remanence-parity/src/lib.rs crates/remanence-parity/src/sidecar.rs @ 7fb10f8 -->
## Parity scheme

Erasure coding is Reed-Solomon over GF(2^8) with a Cauchy matrix; the
scheme id written to tape is `rs-cauchy-gf256-v1`. A scheme is the triple
(data blocks per stripe, parity blocks per stripe, stripes per
neighborhood). The defaults at the standard 256 KiB block size:

| Scheme | k | m | Stripes/neighborhood | Tolerance |
|---|---|---|---|---|
| `default` | 128 | 4 | 512 | ~512 MiB of loss per neighborhood |
| `conservative` | 64 | 6 | 256 | ~384 MiB, higher parity overhead |
| `none` | — | — | — | bootstrap written with a no-parity flag |
| `custom:k,m,S` | k | m | S | operator-chosen |

Parity-protected writes require LTO hardware compression disabled on the
drive; compression would decouple logical block counts from physical
media, and the stripe geometry is physical.

<!-- code-anchor: crates/remanence-format/src/model.rs crates/remanence-format/src/layout.rs crates/remanence-format/src/writer.rs @ 7fb10f8 -->
## The stored object: rao-v1

A plaintext stored object is a POSIX pax tar archive — the format id is
`rao-v1`, schema version `1.0` (`1.1` when per-entry xattrs are present).
There is no custom binary header: identity travels in a pax global
extended header with `REMANENCE.*` keys (`format_id`, `schema_version`,
`object_id`, `caller_object_id`, `chunk_size`, `encryption`,
`write_timestamp`, `metadata_preservation`). Each member carries
`REMANENCE.file_id`, `REMANENCE.file_sha256`, and chunk-alignment padding
so that every member's data starts on a chunk boundary (default chunk
size 262144 bytes). The last member is a deterministic CBOR manifest at
`_remanence/manifest.cbor`, followed by tar end-of-archive records.

The consequence worth stating plainly: a plaintext rao-v1 object is
extractable with stock `tar` on any Unix system, with the Remanence
metadata visible as pax headers. The 30-year-readability property is not
a promise, it is the format.

<!-- code-anchor: crates/remanence-aead/src/header.rs crates/remanence-aead/src/stream.rs crates/remanence-aead/src/kdf.rs @ 7fb10f8 -->
## The encrypted envelope: RAO1

An encrypted object wraps the same tar byte stream in an AEAD envelope:

- 128-byte plaintext header, magic `RAO1`, format version 1, cipher-suite
  id 0x01 (HKDF-SHA-256 + ChaCha20-Poly1305), chunk size, 16-byte key id,
  16-byte HKDF salt, and the object id.
- An authenticated metadata frame, then the payload as an age-style
  STREAM: each chunk is `chunk_size` bytes of ciphertext plus a 16-byte
  tag, with an 11-byte counter nonce whose final byte flags the last
  chunk. Truncation is therefore detectable.
- A 16-byte plaintext footer, `RAO1_STREAM_END.`, then zero-fill to a
  chunk-size multiple.

Keys derive from a 32-byte root key through HKDF with the labels
`rao1-salt-v1`, `rao1-object-v1`, `rao1-metadata-v1`, `rao1-payload-v1`.
The key id in the header names the root key; Remanence never stores key
material.

<!-- code-anchor: crates/remanence-parity/src/bootstrap.rs crates/remanence-state/src/index.rs @ 7fb10f8 -->
## Tape identity

A tape's durable identity is the 16-byte UUID in its bootstrap at BOT,
written once at initialization. The barcode (voltag) is deliberately not
written to tape — barcodes are library-inventory labels, and the binding
voltag ↔ tape UUID lives in the catalog's `tapes` table. This is what
makes identity library-independent: move a cartridge to another library
and it is still the same tape. It is also the root of the known
recycle-skew issue when something outside Remanence rewrites a cartridge
under an existing barcode (see
[troubleshooting](guide-troubleshooting.md#known-open-issue)).

<!-- code-anchor: crates/remanence-state/src/index.rs crates/remanence-state/src/paths.rs @ 7fb10f8 -->
## On disk: the rebuildable state

The host-side state, for completeness (paths are operator-configured; see
the [configuration reference](reference-configuration.md)):

- **Per-tape journals** (`<tape-uuid>.remjournal`) — the durable
  disk-side record of what was committed to each tape.
- **Audit segments** (daily `.remaudit` files) — append-only record of
  every state-changing operation, fsynced by default.
- **SQLite index** — schema version 12, tracked via `PRAGMA
  user_version`, with tables for tapes, pools, tape files, objects and
  copies, catalog units, sessions, operations, idempotency keys, media
  readiness, tape-I/O fences, drives, cleaning runs, and alarms. It is a
  projection: `rem rebuild-catalog-from-journals` regenerates it from the
  journals and audit log.
- **Per-tape catalog caches** — regenerable per-tape files under the
  configured cache directory.
