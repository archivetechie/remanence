# Per-tape log reference fixtures

Host per-tape logs (`<tape-uuid>.remtape`, format version 2) for testing an
independent decoder. The Rust authority writer produced every healthy log from
fixed inputs; the damaged tails are cut or edited from a healthy log afterwards.
These are host authority files, not tape images, and carry no on-tape bytes.

Each file starts with its header: magic, version, tape UUID, incarnation,
geometry length, block size, geometry and a CRC-64/XZ. Frames follow. Each frame
has a 28-byte prefix with its own CRC, then structural and payload sections,
each with a CRC-64/XZ trailer. Sequences start at 1 and are contiguous. A group
is authoritative only once its closing record (any tag other than PayloadContinuation) has been decoded.

Tail classes:

- complete: every byte belongs to a complete frame.
- torn: an EOF-short or zero-filled suffix after the last closing record. It is
  not authority; the next exclusive lease removes it before appending.
- recovery required: bytes that are neither authority nor a permitted torn
  suffix. Only an explicit operator repair may cut them.

Each table lists every frame that decodes, in order. Frames past the closed
end belong to a group that never closed: they decode but are not authority.
W is the protected data ordinal, T the committed data ordinal. The counters
describe the authoritative prefix only. `FRAMES.tsv` lists every decoded frame;
`MANIFEST.tsv` lists each file's size, digest, header and counters. Both, and
this README, are generated: `cargo test -p remanence-state --lib
reference_tape_logs` fails if any checked-in byte differs. To regenerate, set
`TAPE_LOG_FIXTURE_EXPORT_DIR` to an empty directory, run that test, and copy
the exported files here.

## parity-off-two-objects.remtape

Parity-off tape: Initialized and two Object barriers.

- bytes: 1867, SHA-256 f68db2b01939a3eec8285e206a9154b958021784a0f67ecb00754cafc21a5971
- header: tape UUID a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1, block size 262144, scheme none, 51 bytes
- tail: complete
- authoritative prefix: last closed sequence 7, closed end 1867
- counters: objects 2, next file 3, W 0, T 6, check 0000000000000000 (Empty), next epoch 0, cursor 0:10, sealed false

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 51 | 150 |
| 2 | PayloadContinuation (2) | 150 | 343 |
| 3 | PayloadContinuation (2) | 343 | 824 |
| 4 | Committed (3) | 824 | 1007 |
| 5 | PayloadContinuation (2) | 1007 | 1200 |
| 6 | PayloadContinuation (2) | 1200 | 1683 |
| 7 | Committed (3) | 1683 | 1867 |

## parity-object-retained-close.remtape

Parity tape: one Object barrier leaving its epoch open, then a retained close publishing the epoch's sidecar.

- bytes: 1362, SHA-256 b2307f01fc98ef040c30cc53ec0e9ee7d92253d771ff7d19680bffd6f2db4bdf
- header: tape UUID a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2, block size 262144, scheme checkpoint-partial-epoch k=8 m=2 S=1, 84 bytes
- tail: complete
- authoritative prefix: last closed sequence 6, closed end 1362
- counters: objects 1, next file 3, W 3, T 3, check 0000000000000000 (Empty), next epoch 1, cursor 0:8, sealed false

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 84 | 216 |
| 2 | PayloadContinuation (2) | 216 | 409 |
| 3 | PayloadContinuation (2) | 409 | 890 |
| 4 | Committed (3) | 890 | 1073 |
| 5 | PayloadContinuation (2) | 1073 | 1179 |
| 6 | Committed (3) | 1179 | 1362 |

## sealed-terminal.remtape

Sealed parity tape: an Object and sidecar barrier, an automatic FinalizationDecided plan, TerminalPrefix, five ComponentWritten records and Sealed.

- bytes: 3426, SHA-256 fa1984fa072dcb5e4bb15cd5f7c98d90c680b847d54aa41854ede6504f004462
- header: tape UUID a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3, block size 262144, scheme checkpoint-partial-epoch k=8 m=2 S=1, 84 bytes
- tail: complete
- authoritative prefix: last closed sequence 21, closed end 3426
- counters: objects 1, next file 9, W 3, T 3, check 0000000000000000 (Empty), next epoch 1, cursor 0:8220, sealed true

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 84 | 216 |
| 2 | PayloadContinuation (2) | 216 | 409 |
| 3 | PayloadContinuation (2) | 409 | 890 |
| 4 | PayloadContinuation (2) | 890 | 996 |
| 5 | Committed (3) | 996 | 1179 |
| 6 | PayloadContinuation (2) | 1179 | 1285 |
| 7 | PayloadContinuation (2) | 1285 | 1387 |
| 8 | FinalizationDecided (7) | 1387 | 1666 |
| 9 | PayloadContinuation (2) | 1666 | 1772 |
| 10 | TerminalPrefix (4) | 1772 | 1941 |
| 11 | PayloadContinuation (2) | 1941 | 2047 |
| 12 | ComponentWritten (5) | 2047 | 2217 |
| 13 | PayloadContinuation (2) | 2217 | 2325 |
| 14 | ComponentWritten (5) | 2325 | 2497 |
| 15 | PayloadContinuation (2) | 2497 | 2605 |
| 16 | ComponentWritten (5) | 2605 | 2777 |
| 17 | PayloadContinuation (2) | 2777 | 2887 |
| 18 | ComponentWritten (5) | 2887 | 3059 |
| 19 | PayloadContinuation (2) | 3059 | 3167 |
| 20 | ComponentWritten (5) | 3167 | 3339 |
| 21 | Sealed (6) | 3339 | 3426 |

## torn-tail.remtape

The parity-off tape with a third group whose closing frame is cut short by EOF. The first two groups remain authoritative.

- bytes: 2574, SHA-256 0d9669f6f7e708a64b97aecdff408ea082c518e245fd35f6c68815073de9486c
- header: tape UUID a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1, block size 262144, scheme none, 51 bytes
- tail: torn: short_body at byte 2543
- authoritative prefix: last closed sequence 7, closed end 1867
- counters: objects 2, next file 3, W 0, T 6, check 0000000000000000 (Empty), next epoch 0, cursor 0:10, sealed false

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 51 | 150 |
| 2 | PayloadContinuation (2) | 150 | 343 |
| 3 | PayloadContinuation (2) | 343 | 824 |
| 4 | Committed (3) | 824 | 1007 |
| 5 | PayloadContinuation (2) | 1007 | 1200 |
| 6 | PayloadContinuation (2) | 1200 | 1683 |
| 7 | Committed (3) | 1683 | 1867 |
| 8 | PayloadContinuation (2) | 1867 | 2060 |
| 9 | PayloadContinuation (2) | 2060 | 2543 |

## zero-filled-tail.remtape

The two-Object parity-off log followed by 4096 zero bytes, as a filesystem may leave an unsynced append after power loss.

- bytes: 5963, SHA-256 bc35cc32c73f7251ae5ffea6dd77c52e45615069e00460d9e4d43f6ab1b754ab
- header: tape UUID a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1, block size 262144, scheme none, 51 bytes
- tail: torn: zero_filled at byte 1867
- authoritative prefix: last closed sequence 7, closed end 1867
- counters: objects 2, next file 3, W 0, T 6, check 0000000000000000 (Empty), next epoch 0, cursor 0:10, sealed false

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 51 | 150 |
| 2 | PayloadContinuation (2) | 150 | 343 |
| 3 | PayloadContinuation (2) | 343 | 824 |
| 4 | Committed (3) | 824 | 1007 |
| 5 | PayloadContinuation (2) | 1007 | 1200 |
| 6 | PayloadContinuation (2) | 1200 | 1683 |
| 7 | Committed (3) | 1683 | 1867 |

## recovery-required-tail.remtape

The two-Object parity-off log with one bit flipped in the last closing frame's structural section. Only the first group is authoritative.

- bytes: 1867, SHA-256 502248b03eb93e4e7bb3ad396ab7994199d9935ec10b88425d61d4c646fe1c7b
- header: tape UUID a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1, block size 262144, scheme none, 51 bytes
- tail: recovery required at byte 1683, sequence 7: bad Structural CRC
- authoritative prefix: last closed sequence 4, closed end 1007
- counters: objects 1, next file 2, W 0, T 3, check 0000000000000000 (Empty), next epoch 0, cursor 0:6, sealed false

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 51 | 150 |
| 2 | PayloadContinuation (2) | 150 | 343 |
| 3 | PayloadContinuation (2) | 343 | 824 |
| 4 | Committed (3) | 824 | 1007 |
| 5 | PayloadContinuation (2) | 1007 | 1200 |
| 6 | PayloadContinuation (2) | 1200 | 1683 |
