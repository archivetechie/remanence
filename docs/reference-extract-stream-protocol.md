<!-- code-anchor: crates/remanence-cli/src/lib.rs crates/remanence-format/src/envelope.rs crates/remanence-aead/src/open.rs crates/remanence-aead/src/range.rs crates/remanence-aead/src/header.rs @ 8238b7b3 -->
# `rem archive extract-stream` protocol

This page is for someone writing a restore agent: a program that reads an
encrypted object from tape or from a file, pipes the stored bytes through
Remanence to decrypt them, and passes the plaintext on. It specifies the input,
output, exit status and memory use of two commands. `rem archive
extract-stream` decrypts an encrypted REM-OBJECT object read from standard
input and writes the plaintext to standard output. `rem archive
covering-range` says which stored bytes a program must fetch to read one part
of the plaintext without reading the whole object. `rem-debug archive` has the
same two commands with the same behaviour.

Both apply to objects sealed by REM-ENCRYPT 1.0, whose header carries
`format_version = 2`; that is the only encrypted form Remanence reads. Both
commands work on local bytes only. They never open a tape drive or the daemon.

## The stored object

An encrypted object is stored as a sequence of parts, and the commands below
refer to them by name.

| Part | Size | Remarks |
| --- | --- | --- |
| Header | 128 bytes | Plaintext. It names the object, the chunk size, and the lengths of the next two frames. |
| Key frame | Given in the header | One slot per recipient, each holding the object's data-encryption key (DEK) wrapped for that recipient. A slot is identified by its [recipient epoch](reference-glossary.md#recipient-epoch). |
| Metadata frame | Given in the header | Authenticated and encrypted. It records the plaintext size and the plaintext's SHA-256. |
| Payload chunks | Chunk size plus 16 bytes each | The plaintext object, cut into chunks of the chunk size, each encrypted and followed by a 16-byte Poly1305 authentication tag. |
| Footer and zero fill | To the next multiple of the chunk size | Marks completion. |

The header, key frame and metadata frame together are the authenticated
prefix. A frame is one authenticated unit of the stored bytes. The plaintext
object is the REM-OBJECT tar stream inside the envelope, and `--range` counts
offsets into it.

## Invocation

```text
rem archive extract-stream [OPTIONS]
rem archive covering-range [OPTIONS] --object-id <ID> --file-id <ID> --range <START:LEN>
```

`extract-stream` has two modes. Whole-object mode reads the complete stored
object. Ranged-ciphertext mode reads only the stored chunks that cover a
requested plaintext range, and is selected by giving `--range`,
`--authenticated-prefix` and `--stored-range-start` together.

| Option | Commands | Value |
| --- | --- | --- |
| `--private-key <PATH>` | both | The recipient's private key, a canonical REMP file ([recipient key-file formats](reference-cli.md#recipient-key-file-formats)), the same file `rem archive extract --private-key` takes. The help lists it as optional, but both commands refuse to run without it. The REMP file carries its recipient epoch id; opening uses the key-frame slot with that epoch id and fails when the object has none. |
| `--range <START:LEN>` | both | A plaintext byte range: the offset of the first byte, a colon, and the number of bytes. It selects `[START, START + LEN)` counted from the start of the plaintext object, not from the start of a member. Each number is a decimal integer and may carry a binary suffix `K`, `KB` or `KiB` (times 1024) or `M`, `MB` or `MiB` (times 1048576), so `256K:44` is `262144:44`. |
| `--object-id <ID>` | `covering-range` | Any identifier the calling program uses for the object, for example its catalog identifier. The command does not compare it with anything and echoes it back as `object_id`. The object's own identifier, from the authenticated header, is returned separately as `envelope_object_id`. |
| `--file-id <ID>` | `covering-range` | Any identifier the calling program uses for the member. It is echoed back as `file_id`. |
| `--authenticated-prefix <PATH>` | `extract-stream` | A file whose first bytes are the object's authenticated prefix. Only the prefix is read, so the stored object itself, or its first chunk, also works. |
| `--stored-range-start <BYTE>` | `extract-stream` | The offset in the stored object at which the bytes on standard input begin. It is the `stored_range_start` that `covering-range` printed. |

A member's place in the plaintext object is in the build report of `rem
archive build`: each entry of `files` has a `data_offset` and a `size_bytes`.

## Whole-object mode

Standard input is exactly one complete stored object, from the first byte of
the header to the last byte of zero fill. The command reads it in order and
never seeks. Bytes after the zero fill are an error (`trailing data after
REM-OBJECT object`).

Standard output receives plaintext only. Without `--range` it is the complete
plaintext object; with `--range` it is the requested interval. Either way the
command reads and validates the whole stored object. No JSON, progress or
diagnostic text is written to standard output.

On success, after the whole object has validated and standard output has been
flushed, the command writes one JSON line to standard error. Its fields are
`command` (`"archive extract-stream"`), `status` (`"ok"`), `object_id`,
`recipient_epochs` (each slot's `epoch_id` and `label`), `format_version`,
`chunk_size`, `stored_size_bytes`, `plaintext_size_bytes`, `plaintext_sha256`,
`bytes_written`, and `range` (`null`, or `start` and `len`). `rem` adds
`observed_endpoints` and `supported_endpoint_kinds` to every JSON line it
prints; `rem-debug` does not.

On failure the command writes one line to standard error beginning
`error: archive extract-stream:`.

Standard output is a stream, not a transaction. Each chunk's plaintext is
written as soon as its tag verifies, so a later failure is reported after
earlier plaintext has already been written. Such a failure can be truncated
input, a bad tag, a bad footer or fill, a wrong plaintext size or digest,
trailing bytes, a range past the end of the plaintext, or a failed write. Standard output never holds
plaintext from a chunk whose tag failed. The consumer discards or invalidates
what it received unless the command exits with status 0.

## Exit status

| Status | Meaning |
| --- | --- |
| 0 | The whole stored object validated, including the final chunk, footer, fill, end of input, plaintext size and plaintext digest; standard output was flushed; the JSON line was written. In ranged-ciphertext mode, every covering chunk validated (see below). |
| 1 | Any failure after the arguments were parsed: a missing or unreadable private key, no matching recipient slot, a validation or authentication failure, truncated input, a bad range, a failed read of standard input, or a failed write or flush of standard output. Whatever reached standard output must not be kept. |
| 2 | The arguments could not be parsed; nothing was read. |

## Memory and backpressure

The command reads one stored chunk, authenticates it, and writes its plaintext
before it reads the next. A program that stops reading standard output
therefore stops the command from reading standard input. Memory holds the key
frame, the metadata frame and a few chunk-sized buffers. It does not grow with
the size of the object.

In whole-object mode, `--range` selects part of the plaintext as it streams
out; it does not reduce the bytes that must be supplied on standard input.
Ranged-ciphertext mode exists for that. Neither mode selects a tar member by
path; the caller supplies the range.

## covering-range

`covering-range` reads exactly the authenticated prefix from standard input:
the 128-byte header, then the key frame and metadata frame whose lengths the
header gives. It reads nothing after them, so piping the whole stored object
in works. It unwraps the DEK with the private key, authenticates the prefix,
and prints one JSON line on standard output with these fields:

| Field | Meaning |
| --- | --- |
| `object_id`, `file_id` | The two identifiers given on the command line, unchanged. |
| `envelope_object_id` | The object identifier from the authenticated header. |
| `plaintext_start`, `plaintext_len` | The requested range. |
| `first_chunk`, `chunk_count` | The first payload chunk that overlaps the range, and how many chunks do. |
| `stored_range_start`, `stored_range_len`, `stored_range_end` | The contiguous stored bytes to fetch: the covering chunks with their tags. `stored_range_end` is exclusive. |
| `authenticated_prefix_len` | The length of the authenticated prefix, the bytes to keep for `--authenticated-prefix`. |

For an empty range (`LEN` of 0), `first_chunk`, `stored_range_start` and
`stored_range_end` are `null`, and `chunk_count` and `stored_range_len` are 0.
An empty range needs no stored bytes, so fetch nothing and do not run
`extract-stream`. A range that ends past the plaintext is refused with
`plaintext range extends past object end`, exit status 1.

The mapping from plaintext to stored bytes is computed by
`remanence_aead::covering_stored_range`, beside the code that lays the chunks
out. A consumer uses the printed values and does not reproduce the
arithmetic, which depends on the frame lengths and the tag size.

## Ranged-ciphertext mode

With `--range`, `--authenticated-prefix` and `--stored-range-start` together,
standard input is exactly the covering stored chunks that `covering-range`
named, starting at `stored_range_start`. It holds no prefix, footer, fill or
unrequested chunk. Bytes after the covering chunks are not read. Giving only
one or two of the three options is an argument error (exit status 2).

The command reads the prefix from the `--authenticated-prefix` file and
authenticates it before it releases any plaintext. The `--stored-range-start`
value must equal the start that the prefix and range imply; otherwise the
command refuses with `ranged input starts at stored byte <BYTE>, expected
<BYTE>`. It then reads and authenticates one chunk at a time, trims the first
and last chunks to the requested range, and writes the selected bytes.

The success line on standard error has `mode` set to `"ranged-ciphertext"`
and these fields: `command`, `status`, `object_id`, `recipient_epochs`,
`chunk_size`, `plaintext_size_bytes`, `plaintext_sha256`, `bytes_written`,
`authenticated_chunks`, `stored_range_start`, `stored_range_len` and `range`.
`plaintext_size_bytes` and `plaintext_sha256` describe the whole object, as
the authenticated metadata records them. This mode does not compute the
digest, and it does not validate the chunks it was not given, the footer or
the fill. A failure in a covering chunk can leave plaintext from earlier
chunks on standard output; as in whole-object mode, the consumer keeps the
output only after exit status 0.

A program that reads several members of one object (a bundle) opens one
ranged read per member. There is no mode that reads several members in one
stream, because each read must know its own chunk numbers and whether its
last chunk is the object's final chunk. The bytes fetched and the memory used
grow with the chunks that cover each range, not with the size of the object.

## Example: one member of an encrypted object

The steps below read one small file out of an encrypted object while fetching
one chunk of payload instead of the whole object. The object was built as in
the [Quickstart](guide-quickstart.md), from a directory `src` holding
`notes.txt` (44 bytes) and `scan-0001.raw` (600000 bytes), sealed to the keys
`primary` and `recovery`:

```sh
rem archive build --inputs src --out demo-enc.rem-object \
    --recipient primary.remr --recipient recovery.remr > build.json
```

The build report gives the object id and, for each file, its id, its
`data_offset` in the plaintext object and its size. Trimmed and formatted:

```text
{
  "chunk_size": 262144,
  "files": [
    {
      "data_offset": 262144,
      "file_id": "9a3807660171c1a19baa70a78cd8e10d74b7e24473450f51d0f48f1aa811962c",
      "path": "notes.txt",
      "size_bytes": 44,
      ...
    },
    ...
  ],
  "object_id": "f58684f3-4c89-4b58-b054-d06b5cf6455f",
  "stored_size_bytes": 1835008,
  ...
}
```

Ask which stored bytes cover `notes.txt`, the range `262144:44`. Here the
whole stored object is on standard input, and the command reads only its
prefix:

```sh
rem archive covering-range --private-key primary.remp \
    --object-id f58684f3-4c89-4b58-b054-d06b5cf6455f \
    --file-id 9a3807660171c1a19baa70a78cd8e10d74b7e24473450f51d0f48f1aa811962c \
    --range 262144:44 < demo-enc.rem-object
```

The answer, formatted:

```text
{
  "authenticated_prefix_len": 2588,
  "chunk_count": 1,
  "command": "archive covering-range",
  "envelope_object_id": "f58684f3-4c89-4b58-b054-d06b5cf6455f",
  "file_id": "9a3807660171c1a19baa70a78cd8e10d74b7e24473450f51d0f48f1aa811962c",
  "first_chunk": 1,
  "object_id": "f58684f3-4c89-4b58-b054-d06b5cf6455f",
  "observed_endpoints": [],
  "plaintext_len": 44,
  "plaintext_start": 262144,
  "status": "ok",
  "stored_range_end": 526908,
  "stored_range_len": 262160,
  "stored_range_start": 264748,
  "supported_endpoint_kinds": ["library", "standalone"]
}
```

Keep the first 2588 bytes as the prefix, and fetch the 262160 stored bytes
that start at offset 264748. A restore agent reads them from tape; with a
local file, `dd` does the same:

```sh
head -c 2588 demo-enc.rem-object > prefix.bin
dd if=demo-enc.rem-object of=frames.bin iflag=skip_bytes,count_bytes \
    skip=264748 count=262160 status=none
```

Decrypt the range from those bytes alone:

```sh
rem archive extract-stream --private-key primary.remp --range 262144:44 \
    --authenticated-prefix prefix.bin --stored-range-start 264748 \
    < frames.bin > notes.txt
```

Standard output holds the 44 bytes of `notes.txt`. Standard error holds the
report, formatted:

```text
{
  "authenticated_chunks": 1,
  "bytes_written": 44,
  "chunk_size": 262144,
  "command": "archive extract-stream",
  "mode": "ranged-ciphertext",
  "object_id": "f58684f3-4c89-4b58-b054-d06b5cf6455f",
  "observed_endpoints": [],
  "plaintext_sha256": "e4052318d519e5ec334a9898fccf181a8b252af14ab70943fc9307ba15d735d5",
  "plaintext_size_bytes": 1572864,
  "range": {"len": 44, "start": 262144},
  "recipient_epochs": [
    {"epoch_id": "fac0215ed1b1b5485d509e69129dfc45", "label": "primary"},
    {"epoch_id": "b49af3f26bc020ef2134af5282247484", "label": "recovery"}
  ],
  "status": "ok",
  "stored_range_len": 262160,
  "stored_range_start": 264748,
  "supported_endpoint_kinds": ["library", "standalone"]
}
```

The same range can be read in whole-object mode, which reads all 1835008
stored bytes and also checks the footer and the whole-object digest:

```sh
rem archive extract-stream --private-key primary.remp --range 262144:44 \
    < demo-enc.rem-object > notes.txt
```

If `--stored-range-start` is wrong, nothing is decrypted:

```text
error: archive extract-stream: decrypt ranged REM-OBJECT stream: REM-OBJECT AEAD envelope failed: invalid REM-OBJECT sealing input: ranged input starts at stored byte 264700, expected 264748
```

## See also

- [`rem archive extract-stream`](reference-cli.md#rem-archive-extract-stream)
  and [`rem archive covering-range`](reference-cli.md#rem-archive-covering-range)
  in the CLI reference.
- [Partial-file restore on LTO](pfr-reference.md), on reading one range of a
  file from tape.
- REM-ENCRYPT §6, "Partial File Restore", which defines the mapping from
  plaintext chunks to stored bytes, in the
  [REM-ENCRYPT specification](../specs/publication/rem-encrypt-1-specification.md).
