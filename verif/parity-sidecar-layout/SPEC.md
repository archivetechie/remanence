# parity-sidecar-layout formal specification

Target: `verif/parity-sidecar-layout/src/lib.rs`, a dependency-free extraction
of the pure binary-layout and CRC-window arithmetic in
`crates/remanence-parity/src/sidecar.rs`.

This proof target covers layout facts only. It deliberately does not prove
HMAC-SHA256 magic derivation, SHA-256 canonical metadata hashing, CRC-64/XZ
algebra, `Vec` allocation, slice copying, tape IO, or Reed-Solomon recovery.

## S1 -- sidecar header block fixed fields

For every block size at least `SIDECAR_HEADER_LEN + 8 = 0xC0`, the extracted
header layout succeeds and returns the sidecar header's fixed ranges, including
the header CRC field at `[0xB0, 0xB8)` and the inline-index range
`[0xB8, block_size - 8)`.

## S2 -- sidecar header CRC windows

For every valid sidecar header block size:

- `header_crc64` covers exactly `[0x00, 0xB0)`
- `header_crc64` is stored at exactly `[0xB0, 0xB8)`
- the block-0 trailing CRC covers exactly `[0x00, block_size - 8)`
- the block-0 trailing CRC is stored at exactly `[block_size - 8, block_size)`

## S3 -- sidecar footer locator fixed fields

For every block size at least `SIDECAR_FOOTER_LEN = 0x80`, the extracted footer
layout succeeds and returns the footer locator's fixed ranges, including the
footer CRC field at `[0x78, 0x80)` and footer padding at `[0x80, block_size)`.

## S4 -- sidecar footer CRC window

For every valid footer block size:

- `footer_crc64` covers exactly `[0x00, 0x78)`
- `footer_crc64` is stored at exactly `[0x78, 0x80)`
- bytes after `0x80` are padding and are outside the footer CRC window

## S5 -- sidecar tape-file block layout

For every positive header/index block count `H` and every parity block count
`P`, if `H + P + H + 1` does not overflow `u64`, the sidecar tape file layout
is:

```text
primary header/index copy: [0, H)
raw parity shard blocks:   [H, H + P)
tail header/index copy:    [H + P, H + P + H)
footer locator block:      H + P + H
total block count:         H + P + H + 1
```

This is the scalar contract behind the production identity
`block_count == 2 * H + P + 1`.

## Trust anchor

The Lean type checker (`lake build` with zero local placeholders) is the trust
anchor. The Rust `drift_guard` test ties this extraction back to the production
sidecar constants, offset slices, and CRC ranges; if it fires, the extraction
and proofs must be re-established.
