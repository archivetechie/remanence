# `rem archive extract-stream` protocol

Status: current (RM0.3a; RM0.3b integration contract)

## Invocation

```text
rem archive extract-stream --key-file PATH [--range START:LEN]
```

`PATH` uses the same key-file contract as the existing encrypted
`rem archive extract`: the file must contain exactly 32 raw root-key bytes.
There is no alternate key encoding or key source.

## Streams

- **stdin:** exactly one complete encrypted RAO1 stored-object byte string,
  from its 128-byte header through its authenticated payload, completion
  footer, and zero fill. Trailing bytes are an error. Input is consumed
  sequentially; the helper does not seek.
- **stdout:** decrypted canonical plaintext bytes only. Without `--range`,
  this is the complete plaintext RAO object. With `--range`, this is the
  absolute plaintext interval `[START, START + LEN)`. The helper still
  consumes and validates the entire encrypted object. No JSON, progress, or
  diagnostics are written to stdout.
- **stderr on success:** one compact JSON line, written only after the whole
  encrypted object validates and stdout flushes. Fields are `command`,
  `status`, `object_id`, `key_id`, `chunk_size`, `stored_size_bytes`,
  `plaintext_size_bytes`, `plaintext_sha256`, `bytes_written`, and `range`.
- **stderr on failure:** a human-readable line beginning
  `error: archive extract-stream:`. Clap writes argument errors and usage to
  stderr before the helper starts.

stdout is deliberately streaming rather than transactional. On an
authentication, truncation, footer, fill, digest, downstream-write, or range
validation failure, stdout can contain plaintext from earlier authenticated
chunks. It never contains plaintext from a chunk whose Poly1305 tag failed.
The consumer must discard or invalidate its destination unless the helper
exits successfully.

## Exit codes

- `0`: the complete encrypted object, including final-chunk nonce finality,
  footer, fill, EOF, plaintext size, and plaintext digest, validated; stdout
  flushed; the success JSON line was written to stderr.
- `1`: key-file, RAO validation/authentication, truncation, range, stdin-read,
  or stdout-write/flush failure. Any stdout is only the already-authenticated
  prefix or selected bytes described above and must not be committed.
- `2`: command-line parsing/usage failure (Clap); no ciphertext is processed.

## Backpressure and memory

The helper passes stdin and stdout directly to `remanence_aead::open`. That
primitive reads one stored payload chunk, authenticates it with the existing
RAO STREAM nonce/finality logic, then writes the corresponding plaintext
chunk. A blocking stdout write therefore backpressures further stdin reads.
Memory is bounded by the AEAD metadata frame and a small constant number of
chunk-sized buffers; it does not scale with object size.

`--range` is a streaming plaintext slice, not a ciphertext range request. It
does not use `cipher_offset`, and it does not reduce the ciphertext bytes that
RM0.3b must feed. Tar member selection by path is not part of RM0.3a.
