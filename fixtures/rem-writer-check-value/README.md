# Writer commit check values

`vectors.json` pins CRC-64/XZ over the concatenation of whole-block CRCs,
each encoded as eight little-endian bytes in ascending data ordinal order.
It covers an empty range, one entry, three entries with byte-order-sensitive
data, and a range derived from four 256 KiB blocks. CRCs and check values use
16-digit lowercase hexadecimal strings.

For the derived range, block `i`, byte `j` is `(i * 31 + j) mod 251`, with
zero-based `i` and `j`. The file records the pattern parameters and every
block CRC so each implementation checks both stages independently.

These vectors belong to the Implementation Guide's writer chapter. They are
not part of the REM-PARITY-1.0 vector set or the on-tape format. The check
detects accidental differences; it provides no protection against deliberate
tampering and imposes no requirement on readers.
