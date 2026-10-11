<!-- code-anchor: crates/remanence-aead crates/remanence-format/src/envelope.rs crates/remanence-cli/src/lib.rs @ bc2f9f90 -->
# `rem archive extract-stream` protocol

`rem archive extract-stream` decrypts an encrypted REM-OBJECT object read from
standard input and writes the plaintext to standard output. It is meant for
programs that pipe stored bytes through it, for example a restore agent that
reads an object from tape and passes it on. Its companion
`rem archive covering-range` tells such a program which stored bytes it must
fetch to read part of one file. This page specifies the input, output, exit
codes, and memory behavior of both. It applies to REM-ENCRYPT 1.0 objects with
on-tape `format_version = 2`, the only encrypted form Remanence reads.

The command has two modes. Whole-object mode reads the complete stored object.
Ranged-ciphertext mode reads only the stored bytes that cover a requested
plaintext range.

## Invocation

```text
rem archive extract-stream --private-key KEY_FILE [--range START:LEN]
rem archive covering-range --private-key KEY_FILE --object-id OBJECT_ID --file-id FILE_ID --range START:LEN
rem archive extract-stream --private-key KEY_FILE --range START:LEN \
  --authenticated-prefix PREFIX_FILE --stored-range-start STORED_START
```

| Placeholder | What to supply |
|---|---|
| `KEY_FILE` | Path of the recipient's private key file in the canonical REMP format ([recipient key-file formats](reference-cli.md#recipient-key-file-formats)). It is the same file that `rem archive extract --private-key` takes. |
| `START:LEN` | A plaintext byte range: the offset of the first byte, a colon, and the number of bytes, both in decimal. `START:LEN` selects the bytes `[START, START + LEN)` counted from the start of the plaintext REM-OBJECT object. |
| `OBJECT_ID` | An identifier that identifies the object to the calling program, for example the object's identifier in its catalog. The command does not interpret it and echoes it back as `object_id`. |
| `FILE_ID` | An identifier that identifies the file to the calling program. The command does not interpret it and echoes it back as `file_id`. |
| `PREFIX_FILE` | Path of a file holding the object's header, key frame, and metadata frame, exactly the bytes `covering-range` read from standard input. The caller keeps them from the `covering-range` call. |
| `STORED_START` | The byte offset in the stored (encrypted) object at which the bytes on standard input begin. Use the `stored_range_start` value that `covering-range` printed. When `LEN` is 0 the printed `stored_range_start` is `null`: an empty range needs no ciphertext, so fetch nothing and do not run `extract-stream`. |

The REMP record carries its recipient epoch id and label. Opening selects the
key-frame slot with that epoch id and fails when the object has no matching
slot. The private key file is the only key source.

## Whole-object streams

- **stdin:** exactly one complete encrypted REMO stored-object byte string,
  from its 128-byte scalar header through the recipient key frame,
  authenticated metadata/payload, completion footer, and zero fill. Trailing
  bytes are an error. Input is consumed sequentially; the command does not seek.
- **stdout:** decrypted canonical plaintext bytes only. Without `--range`,
  this is the complete plaintext REM-OBJECT object. With `--range`, this is the
  absolute plaintext interval `[START, START + LEN)`. The command still
  consumes and validates the entire encrypted object. No JSON, progress, or
  diagnostics are written to stdout.
- **stderr on success:** one compact JSON line, written only after the whole
  encrypted object validates and stdout flushes. Fields are `command`,
  `status`, `object_id`, `recipient_epochs`, `format_version`, `chunk_size`,
  `stored_size_bytes`, `plaintext_size_bytes`, `plaintext_sha256`,
  `bytes_written`, and `range`.
- **stderr on failure:** a human-readable line beginning
  `error: archive extract-stream:`. The argument parser writes argument errors and usage to
  stderr before the command starts.

stdout is deliberately streaming rather than transactional. On an
authentication, truncation, footer, fill, digest, downstream-write, or range
validation failure, stdout can contain plaintext from earlier authenticated
chunks. It never contains plaintext from a chunk whose Poly1305 tag failed.
The consumer must discard or invalidate its destination unless the command
exits successfully.

## Exit codes

- `0`: the complete encrypted object, including final-chunk nonce finality,
  footer, fill, EOF, plaintext size, and plaintext digest, validated; stdout
  flushed; the success JSON line was written to stderr.
- `1`: private-key, epoch-selection, REM-OBJECT validation/authentication,
  truncation, range, stdin-read, or stdout-write/flush failure. Any stdout is
  only the already-authenticated prefix or selected bytes described above and
  must not be committed.
- `2`: command-line parsing or usage failure; no ciphertext is processed.

## Backpressure and memory

The command passes stdin and stdout through the `remanence-format` envelope
funnel, which delegates to `remanence_aead::open`. The opener reads one stored
payload chunk, authenticates it with the REM-OBJECT STREAM nonce/finality logic, then
writes the corresponding plaintext chunk. A blocking stdout write therefore
backpressures further stdin reads. Memory is bounded by the key frame, AEAD
metadata frame, and a small constant number of chunk-sized buffers; it does
not scale with object size.

In whole-object mode `--range` selects a slice of the plaintext as it streams
out. It does not reduce the ciphertext that must be supplied on standard input;
that is the purpose of ranged-ciphertext mode, below. Whole-object mode has no
option to select a tar member by path.

## Covering-range query

An encrypted object is stored as a header, a key frame, a metadata frame, and
then the payload in fixed-size authenticated pieces called chunks. A frame is
one such authenticated unit of the stored bytes. To read part of one file, the
caller needs the header, key frame, and metadata frame (the prefix) and the
chunks that overlap the range. `covering-range` computes which stored bytes
those are.

`archive covering-range` reads exactly the REM-OBJECT scalar header, required key
frame, and metadata frame from stdin. It selects the matching recipient epoch,
authenticates that prefix with the unwrapped DEK, and emits one JSON line on
stdout. The storage `object_id` and caller's `file_id` are opaque and echoed
for request correlation; authenticated header identity is returned separately
as `envelope_object_id`. The response includes `plaintext_start`,
`plaintext_len`, `first_chunk`, `chunk_count`, `stored_range_start`,
`stored_range_len`, `stored_range_end` (exclusive), and
`authenticated_prefix_len`.

The plaintext-to-ciphertext mapping is computed by
`remanence_aead::covering_stored_range`, which uses the format's `cipher_offset`
implementation. Consumers must not reproduce the tag-padding,
key-frame, or metadata-frame arithmetic.

## Ranged-ciphertext streams

When both `--authenticated-prefix` and `--stored-range-start` accompany
`--range`, stdin changes to exactly the contiguous covering ciphertext frames
returned by `covering-range`; it does not contain the prefix, footer, fill, or
unrequested payload. The prefix path is reread and authenticated before any
plaintext is released. The supplied stored start must exactly equal the
start that `covering-range` computes.

The command reads and authenticates one stored chunk at a time, applies edge
trimming only to the first and last plaintext chunks, and writes the selected
bytes to stdout. Its success JSON has `mode: "ranged-ciphertext"`,
`recipient_epochs`, `authenticated_chunks`, and the stored range geometry. It
authenticates the key-frame-bound metadata and every covering payload frame,
but deliberately does not validate unfetched frames, the footer, fill, or
whole-object plaintext digest. A covering-frame failure can leave output from
earlier authenticated frames; the consumer commits output only after
exit status 0, as in whole-object mode.

A program that reads several members of one bundle (an object that holds many files) opens one independent stored range per member.
There is no single-stream multi-member mode: each open has unambiguous object
chunk indices and final-chunk nonce state. Memory and ciphertext bytes scale
with covering chunks, not with stored object size or member count times object
size.
