# Per-tape log reference fixtures

Host per-tape logs (`<tape-uuid>.remtape`, format version 1) for testing an
independent decoder. The Rust authority writer produced every healthy log from
fixed inputs; the damaged tails are cut or edited from a healthy log afterwards.
These are host authority files, not tape images, and carry no on-tape bytes.

Each file starts with its header: magic, version, tape UUID, incarnation,
geometry length, block size, geometry and a CRC-64/XZ. Frames follow. Each frame
has a 28-byte prefix with its own CRC, then structural and payload sections,
each with a CRC-64/XZ trailer. Sequences start at 1 and are contiguous. A group
is authoritative only once its closing record (any tag other than Initialized
or PayloadContinuation) has been decoded.

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

- bytes: 1795, SHA-256 63cadd56599d4988074c7a06c58b025e3b574447ada10500e50d3ecc294ab321
- header: tape UUID a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1, block size 262144, scheme none, 51 bytes
- tail: complete
- authoritative prefix: last closed sequence 7, closed end 1795
- counters: objects 2, next file 3, W 0, T 6, check 0000000000000000 (Empty), next epoch 0, cursor 0:10, sealed false

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 51 | 150 |
| 2 | PayloadContinuation (2) | 150 | 343 |
| 3 | PayloadContinuation (2) | 343 | 824 |
| 4 | Committed (3) | 824 | 971 |
| 5 | PayloadContinuation (2) | 971 | 1164 |
| 6 | PayloadContinuation (2) | 1164 | 1647 |
| 7 | Committed (3) | 1647 | 1795 |

## parity-object-retained-close.remtape

Parity tape: one Object barrier leaving its epoch open, then a retained close publishing the epoch's sidecar.

- bytes: 1290, SHA-256 3dbad0de12201c63f673a869a0ec7ad23e17c78474abc47c9053aa7b303d8371
- header: tape UUID a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2, block size 262144, scheme checkpoint-partial-epoch k=8 m=2 S=1, 84 bytes
- tail: complete
- authoritative prefix: last closed sequence 6, closed end 1290
- counters: objects 1, next file 3, W 3, T 3, check 0000000000000000 (Empty), next epoch 1, cursor 0:8, sealed false

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 84 | 216 |
| 2 | PayloadContinuation (2) | 216 | 409 |
| 3 | PayloadContinuation (2) | 409 | 890 |
| 4 | Committed (3) | 890 | 1037 |
| 5 | PayloadContinuation (2) | 1037 | 1143 |
| 6 | Committed (3) | 1143 | 1290 |

## sealed-terminal.remtape

Sealed parity tape: an Object and sidecar barrier, an automatic FinalizationDecided plan, TerminalPrefix, five ComponentWritten records and Sealed.

- bytes: 3270, SHA-256 9815af5619f3b4c82a3a2005de7a03acb79383c256d486945164c2e4f8bd56fb
- header: tape UUID a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3, block size 262144, scheme checkpoint-partial-epoch k=8 m=2 S=1, 84 bytes
- tail: complete
- authoritative prefix: last closed sequence 21, closed end 3270
- counters: objects 1, next file 9, W 3, T 3, check 0000000000000000 (Empty), next epoch 1, cursor 0:8220, sealed true

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 84 | 216 |
| 2 | PayloadContinuation (2) | 216 | 409 |
| 3 | PayloadContinuation (2) | 409 | 890 |
| 4 | PayloadContinuation (2) | 890 | 996 |
| 5 | Committed (3) | 996 | 1143 |
| 6 | PayloadContinuation (2) | 1143 | 1249 |
| 7 | PayloadContinuation (2) | 1249 | 1351 |
| 8 | FinalizationDecided (7) | 1351 | 1630 |
| 9 | PayloadContinuation (2) | 1630 | 1736 |
| 10 | TerminalPrefix (4) | 1736 | 1885 |
| 11 | PayloadContinuation (2) | 1885 | 1991 |
| 12 | ComponentWritten (5) | 1991 | 2141 |
| 13 | PayloadContinuation (2) | 2141 | 2249 |
| 14 | ComponentWritten (5) | 2249 | 2401 |
| 15 | PayloadContinuation (2) | 2401 | 2509 |
| 16 | ComponentWritten (5) | 2509 | 2661 |
| 17 | PayloadContinuation (2) | 2661 | 2771 |
| 18 | ComponentWritten (5) | 2771 | 2923 |
| 19 | PayloadContinuation (2) | 2923 | 3031 |
| 20 | ComponentWritten (5) | 3031 | 3183 |
| 21 | Sealed (6) | 3183 | 3270 |

## torn-tail.remtape

The parity-off tape with a third group whose closing frame is cut short by EOF. The first two groups remain authoritative.

- bytes: 2502, SHA-256 b6dc2deca9e43f08fc2d5720e1dffaf2a15f2d33eb812594d2251e5998bea1bb
- header: tape UUID a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1, block size 262144, scheme none, 51 bytes
- tail: torn: short_body at byte 2471
- authoritative prefix: last closed sequence 7, closed end 1795
- counters: objects 2, next file 3, W 0, T 6, check 0000000000000000 (Empty), next epoch 0, cursor 0:10, sealed false

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 51 | 150 |
| 2 | PayloadContinuation (2) | 150 | 343 |
| 3 | PayloadContinuation (2) | 343 | 824 |
| 4 | Committed (3) | 824 | 971 |
| 5 | PayloadContinuation (2) | 971 | 1164 |
| 6 | PayloadContinuation (2) | 1164 | 1647 |
| 7 | Committed (3) | 1647 | 1795 |
| 8 | PayloadContinuation (2) | 1795 | 1988 |
| 9 | PayloadContinuation (2) | 1988 | 2471 |

## zero-filled-tail.remtape

The two-Object parity-off log followed by 4096 zero bytes, as a filesystem may leave an unsynced append after power loss.

- bytes: 5891, SHA-256 f0edccad41d538803c378e923162d91ce79e8a4951695789428abb0ae2ba2ba5
- header: tape UUID a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1, block size 262144, scheme none, 51 bytes
- tail: torn: zero_filled at byte 1795
- authoritative prefix: last closed sequence 7, closed end 1795
- counters: objects 2, next file 3, W 0, T 6, check 0000000000000000 (Empty), next epoch 0, cursor 0:10, sealed false

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 51 | 150 |
| 2 | PayloadContinuation (2) | 150 | 343 |
| 3 | PayloadContinuation (2) | 343 | 824 |
| 4 | Committed (3) | 824 | 971 |
| 5 | PayloadContinuation (2) | 971 | 1164 |
| 6 | PayloadContinuation (2) | 1164 | 1647 |
| 7 | Committed (3) | 1647 | 1795 |

## recovery-required-tail.remtape

The two-Object parity-off log with one bit flipped in the last closing frame's structural section. Only the first group is authoritative.

- bytes: 1795, SHA-256 0a5864eb8e2a792de343c5499764857b1692eedb23bc222bcea5e3b2624300a9
- header: tape UUID a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1, block size 262144, scheme none, 51 bytes
- tail: recovery required at byte 1647, sequence 7: bad Structural CRC
- authoritative prefix: last closed sequence 4, closed end 971
- counters: objects 1, next file 2, W 0, T 3, check 0000000000000000 (Empty), next epoch 0, cursor 0:6, sealed false

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 51 | 150 |
| 2 | PayloadContinuation (2) | 150 | 343 |
| 3 | PayloadContinuation (2) | 343 | 824 |
| 4 | Committed (3) | 824 | 971 |
| 5 | PayloadContinuation (2) | 971 | 1164 |
| 6 | PayloadContinuation (2) | 1164 | 1647 |
