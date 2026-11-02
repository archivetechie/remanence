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

- bytes: 1964, SHA-256 57bae284836432958ebfd4cc0f5b16c3ad7ab69a00d5a086c7c7148fde911b52
- header: tape UUID a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1, block size 262144, scheme none, 51 bytes
- tail: complete
- authoritative prefix: last closed sequence 8, closed end 1964
- counters: objects 2, next file 3, W 0, T 6, check 0000000000000000 (Empty), next epoch 0, cursor 0:10, sealed false

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 51 | 150 |
| 2 | SessionOpened (18) | 150 | 247 |
| 3 | PayloadContinuation (2) | 247 | 440 |
| 4 | PayloadContinuation (2) | 440 | 921 |
| 5 | Committed (3) | 921 | 1104 |
| 6 | PayloadContinuation (2) | 1104 | 1297 |
| 7 | PayloadContinuation (2) | 1297 | 1780 |
| 8 | Committed (3) | 1780 | 1964 |

## parity-object-session-close.remtape

Parity tape: one Object barrier leaving its epoch open, then ClosedOut and SessionEnded publishing the epoch's sidecar and clean end.

- bytes: 1620, SHA-256 7cdc8a92f1180f7cc2abdf02970bbbfd5cf4cde538295f5352186775910cd98e
- header: tape UUID a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2, block size 262144, scheme checkpoint-partial-epoch k=8 m=2 S=1, 84 bytes
- tail: complete
- authoritative prefix: last closed sequence 8, closed end 1620
- counters: objects 1, next file 3, W 3, T 3, check 0000000000000000 (Empty), next epoch 1, cursor 0:12, sealed false

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 84 | 216 |
| 2 | SessionOpened (18) | 216 | 313 |
| 3 | PayloadContinuation (2) | 313 | 506 |
| 4 | PayloadContinuation (2) | 506 | 987 |
| 5 | Committed (3) | 987 | 1171 |
| 6 | PayloadContinuation (2) | 1171 | 1277 |
| 7 | ClosedOut (17) | 1277 | 1462 |
| 8 | SessionEnded (19) | 1462 | 1620 |

## sealed-terminal.remtape

Sealed parity tape: an Object and sidecar barrier, an automatic FinalizationDecided plan, TerminalPrefix, five ComponentWritten records and Sealed.

- bytes: 3524, SHA-256 2932aeedea308d9fe344966bc0f67af37a556a70d703d8a68deab9f1552ec33b
- header: tape UUID a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3, block size 262144, scheme checkpoint-partial-epoch k=8 m=2 S=1, 84 bytes
- tail: complete
- authoritative prefix: last closed sequence 22, closed end 3524
- counters: objects 1, next file 9, W 3, T 3, check 0000000000000000 (Empty), next epoch 1, cursor 0:8220, sealed true

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 84 | 216 |
| 2 | SessionOpened (18) | 216 | 313 |
| 3 | PayloadContinuation (2) | 313 | 506 |
| 4 | PayloadContinuation (2) | 506 | 987 |
| 5 | PayloadContinuation (2) | 987 | 1093 |
| 6 | Committed (3) | 1093 | 1277 |
| 7 | PayloadContinuation (2) | 1277 | 1383 |
| 8 | PayloadContinuation (2) | 1383 | 1485 |
| 9 | FinalizationDecided (7) | 1485 | 1764 |
| 10 | PayloadContinuation (2) | 1764 | 1870 |
| 11 | TerminalPrefix (4) | 1870 | 2039 |
| 12 | PayloadContinuation (2) | 2039 | 2145 |
| 13 | ComponentWritten (5) | 2145 | 2315 |
| 14 | PayloadContinuation (2) | 2315 | 2423 |
| 15 | ComponentWritten (5) | 2423 | 2595 |
| 16 | PayloadContinuation (2) | 2595 | 2703 |
| 17 | ComponentWritten (5) | 2703 | 2875 |
| 18 | PayloadContinuation (2) | 2875 | 2985 |
| 19 | ComponentWritten (5) | 2985 | 3157 |
| 20 | PayloadContinuation (2) | 3157 | 3265 |
| 21 | ComponentWritten (5) | 3265 | 3437 |
| 22 | Sealed (6) | 3437 | 3524 |

## torn-tail.remtape

The parity-off tape with a third group whose closing frame is cut short by EOF. The first two groups remain authoritative.

- bytes: 2671, SHA-256 c03e74bd412f1df303a0b55d9f7ba27e221cfd34ab359f12c0e266b9000483e4
- header: tape UUID a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1, block size 262144, scheme none, 51 bytes
- tail: torn: short_body at byte 2640
- authoritative prefix: last closed sequence 8, closed end 1964
- counters: objects 2, next file 3, W 0, T 6, check 0000000000000000 (Empty), next epoch 0, cursor 0:10, sealed false

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 51 | 150 |
| 2 | SessionOpened (18) | 150 | 247 |
| 3 | PayloadContinuation (2) | 247 | 440 |
| 4 | PayloadContinuation (2) | 440 | 921 |
| 5 | Committed (3) | 921 | 1104 |
| 6 | PayloadContinuation (2) | 1104 | 1297 |
| 7 | PayloadContinuation (2) | 1297 | 1780 |
| 8 | Committed (3) | 1780 | 1964 |
| 9 | PayloadContinuation (2) | 1964 | 2157 |
| 10 | PayloadContinuation (2) | 2157 | 2640 |

## zero-filled-tail.remtape

The two-Object parity-off log followed by 4096 zero bytes, as a filesystem may leave an unsynced append after power loss.

- bytes: 6060, SHA-256 ea207261f2502f1e6bdba354a46b478cd1fa9a9f1abd01762ecc96c186c99496
- header: tape UUID a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1, block size 262144, scheme none, 51 bytes
- tail: torn: zero_filled at byte 1964
- authoritative prefix: last closed sequence 8, closed end 1964
- counters: objects 2, next file 3, W 0, T 6, check 0000000000000000 (Empty), next epoch 0, cursor 0:10, sealed false

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 51 | 150 |
| 2 | SessionOpened (18) | 150 | 247 |
| 3 | PayloadContinuation (2) | 247 | 440 |
| 4 | PayloadContinuation (2) | 440 | 921 |
| 5 | Committed (3) | 921 | 1104 |
| 6 | PayloadContinuation (2) | 1104 | 1297 |
| 7 | PayloadContinuation (2) | 1297 | 1780 |
| 8 | Committed (3) | 1780 | 1964 |

## recovery-required-tail.remtape

The two-Object parity-off log with one bit flipped in the last closing frame's structural section. Only the first group is authoritative.

- bytes: 1964, SHA-256 a8eb4e9b4ebd5a4d86fe9995768a5636904f5023a9b59977d35e76afebc2980c
- header: tape UUID a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1, block size 262144, scheme none, 51 bytes
- tail: recovery required at byte 1780, sequence 8: bad Structural CRC
- authoritative prefix: last closed sequence 5, closed end 1104
- counters: objects 1, next file 2, W 0, T 3, check 0000000000000000 (Empty), next epoch 0, cursor 0:6, sealed false

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 51 | 150 |
| 2 | SessionOpened (18) | 150 | 247 |
| 3 | PayloadContinuation (2) | 247 | 440 |
| 4 | PayloadContinuation (2) | 440 | 921 |
| 5 | Committed (3) | 921 | 1104 |
| 6 | PayloadContinuation (2) | 1104 | 1297 |
| 7 | PayloadContinuation (2) | 1297 | 1780 |
