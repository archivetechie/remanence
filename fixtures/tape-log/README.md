# Per-tape log reference fixtures

Host per-tape logs (`<tape-uuid>.remtape`, format version 3) for testing an
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

## B9 tail wire layout and fold rules

Records use canonical CBOR maps with ascending unsigned integer keys, definite
lengths and shortest integer encodings. SessionEnded (tag 19) has 12 fields;
TailFile (the TailFile continuation chunk) has 7 fields. Existing keys retain
their meanings. New fields are mandatory, including zero, false and null values:

| Record | Key | Field | CBOR type | Meaning |
|---|---:|---|---|---|
| SessionEnded | 9 | synchronized_tail_blocks | unsigned u64 | Durable tail extent in logical blocks, including synchronized filemarks; relative to cursor.lba. |
| SessionEnded | 10 | possible_tail_blocks | unsigned u64 | Sum of known_blocks + unknown_blocks + one for each non-Absent filemark. |
| SessionEnded | 11 | proved_eod | unsigned u64 or null | Absolute physical EOD; null for an ordinary ending, set for a refinement. |
| TailFile | 5 | synchronized_blocks | unsigned u64 | Synchronized prefix of known_blocks. |
| TailFile | 6 | synchronized_filemark | boolean | Whether the filemark is synchronized. |

`session-tail-synchronized.remtape` exercises every new key: file 2 has
known_blocks=2, synchronized_blocks=2 and synchronized_filemark=true; file 3
has known_blocks=4, synchronized_blocks=0 and synchronized_filemark=false.
Both have unknown_blocks=0 and filemark=Present. The ending has keys 9=3,
10=8, 11=null. Its cursor.lba=6 makes the absolute bounds [9,14].

Synchronization must form a prefix: synchronized_blocks cannot exceed
known_blocks. A synchronized filemark requires Present, all known blocks
synchronized and no unknown blocks. After any file without a synchronized
filemark, subsequent files may have neither synchronized blocks nor a
synchronized filemark. Violations report `invalid record: tail synchronization order`.
The synchronized vector exercises the allowed fully synchronized to accepted-only
transition. Tail file numbers are contiguous from next_file; the ordinary ending
binds the continuation group and its exact count and computed bounds.

`session-tail-refined.remtape` repeats that log byte-for-byte, then adds a
SessionEnded with key 11=11, strictly between 9 and 14. This uses
`record_session_tail_eod`: the refinement has no preceding continuation frames,
but retains the original tail GroupRef and every other field of the ending.
It does not rebind the tail to the empty refinement group. A new SessionOpened
then demonstrates reuse. The fold requires an empty continuation group, the
same session position and all original ending fields except proved_eod, no open
session, and EOD within the inclusive synchronized/possible bounds. An existing
proof cannot change. SessionOpened clears the prior ending.

The negative vectors use the production frame encoder on writer-produced logs,
so their CRCs and schema are valid; the fold rejects their semantics. Their exact
errors (including byte offsets and sequences) appear in each section and in the
manifest tail column:

- `session-tail-refinement-rejected/below-synchronized.remtape`: EOD 8 < 9.
- `session-tail-refinement-rejected/reason-mismatch.remtape`: changes reason.
- `session-tail-refinement-rejected/session-open.remtape`: refinement after a
  new session opens, when no previous ending remains available to refine.

For rejected files the frame table includes the rejected frame; counters describe
the validated prefix before it. `reference_tape_logs_production_reader_matches_manifest`
reads every checked-in vector through the production codec and store fold and
compares all manifest fields and frame rows, including exact rejection errors.

## session-tail-refinement-rejected/below-synchronized.remtape

Refinement EOD 8 is below the synchronized bound 9.

- bytes: 1646, SHA-256 439b6fa307c451e76af5ff8dd98e048d24f6b3f7e9ddfd62b74d56b74edd7bd9
- header: tape UUID a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1, block size 262144, scheme none, 51 bytes
- tail: tape log recovery required at byte 1487, sequence Some(9): invalid record: tail EOD refinement
- authoritative prefix: last closed sequence 8, closed end 1487
- counters: objects 1, next file 2, W 0, T 3, check 0000000000000000 (Empty), next epoch 0, cursor 0:6, sealed false

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 51 | 150 |
| 2 | SessionOpened (18) | 150 | 315 |
| 3 | PayloadContinuation (2) | 315 | 508 |
| 4 | PayloadContinuation (2) | 508 | 989 |
| 5 | Committed (3) | 989 | 1173 |
| 6 | PayloadContinuation (2) | 1173 | 1250 |
| 7 | PayloadContinuation (2) | 1250 | 1328 |
| 8 | SessionEnded (19) | 1328 | 1487 |
| 9 | SessionEnded (19) | 1487 | 1646 |

## session-tail-refinement-rejected/reason-mismatch.remtape

Refinement changes Abort to CleanClose.

- bytes: 1646, SHA-256 dc180622e7336f59400fba0dfdad3e7cd435b7b4a3028d67f4742015ed0d9f8e
- header: tape UUID a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1, block size 262144, scheme none, 51 bytes
- tail: tape log recovery required at byte 1487, sequence Some(9): invalid record: tail EOD refinement
- authoritative prefix: last closed sequence 8, closed end 1487
- counters: objects 1, next file 2, W 0, T 3, check 0000000000000000 (Empty), next epoch 0, cursor 0:6, sealed false

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 51 | 150 |
| 2 | SessionOpened (18) | 150 | 315 |
| 3 | PayloadContinuation (2) | 315 | 508 |
| 4 | PayloadContinuation (2) | 508 | 989 |
| 5 | Committed (3) | 989 | 1173 |
| 6 | PayloadContinuation (2) | 1173 | 1250 |
| 7 | PayloadContinuation (2) | 1250 | 1328 |
| 8 | SessionEnded (19) | 1328 | 1487 |
| 9 | SessionEnded (19) | 1487 | 1646 |

## session-tail-refinement-rejected/session-open.remtape

A new SessionOpened clears the previous ending; refinement while open is refused.

- bytes: 1811, SHA-256 07850d9a9c4c4df4f014fb964100a2cf4eb9781e9c48fbb47288bb933c7aba1c
- header: tape UUID a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1, block size 262144, scheme none, 51 bytes
- tail: tape log recovery required at byte 1652, sequence Some(10): invalid record: tail EOD without ending
- authoritative prefix: last closed sequence 9, closed end 1652
- counters: objects 1, next file 2, W 0, T 3, check 0000000000000000 (Empty), next epoch 0, cursor 0:6, sealed false

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 51 | 150 |
| 2 | SessionOpened (18) | 150 | 315 |
| 3 | PayloadContinuation (2) | 315 | 508 |
| 4 | PayloadContinuation (2) | 508 | 989 |
| 5 | Committed (3) | 989 | 1173 |
| 6 | PayloadContinuation (2) | 1173 | 1250 |
| 7 | PayloadContinuation (2) | 1250 | 1328 |
| 8 | SessionEnded (19) | 1328 | 1487 |
| 9 | SessionOpened (18) | 1487 | 1652 |
| 10 | SessionEnded (19) | 1652 | 1811 |

## session-tail-synchronized.remtape

Two tail files: fully synchronized then accepted only. Cursor 6, synchronized tail 3, possible tail 8; absolute bounds [9,14], proved_eod null.

- bytes: 1487, SHA-256 777e13ebb01deafb922dbd085086dfbf1111e7f94ad002874ae8d79ad608de2d
- header: tape UUID a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1, block size 262144, scheme none, 51 bytes
- tail: complete
- authoritative prefix: last closed sequence 8, closed end 1487
- counters: objects 1, next file 2, W 0, T 3, check 0000000000000000 (Empty), next epoch 0, cursor 0:6, sealed false

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 51 | 150 |
| 2 | SessionOpened (18) | 150 | 315 |
| 3 | PayloadContinuation (2) | 315 | 508 |
| 4 | PayloadContinuation (2) | 508 | 989 |
| 5 | Committed (3) | 989 | 1173 |
| 6 | PayloadContinuation (2) | 1173 | 1250 |
| 7 | PayloadContinuation (2) | 1250 | 1328 |
| 8 | SessionEnded (19) | 1328 | 1487 |

## session-tail-refined.remtape

The synchronized log followed by a refinement proving EOD 11 strictly inside [9,14], then SessionOpened for reuse.

- bytes: 1811, SHA-256 ca180579edd67641ce1d589823112f6937e0c44000f39ac654806110d13231fa
- header: tape UUID a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1, block size 262144, scheme none, 51 bytes
- tail: complete
- authoritative prefix: last closed sequence 10, closed end 1811
- counters: objects 1, next file 2, W 0, T 3, check 0000000000000000 (Empty), next epoch 0, cursor 0:6, sealed false

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 51 | 150 |
| 2 | SessionOpened (18) | 150 | 315 |
| 3 | PayloadContinuation (2) | 315 | 508 |
| 4 | PayloadContinuation (2) | 508 | 989 |
| 5 | Committed (3) | 989 | 1173 |
| 6 | PayloadContinuation (2) | 1173 | 1250 |
| 7 | PayloadContinuation (2) | 1250 | 1328 |
| 8 | SessionEnded (19) | 1328 | 1487 |
| 9 | SessionEnded (19) | 1487 | 1646 |
| 10 | SessionOpened (18) | 1646 | 1811 |

## parity-off-two-objects.remtape

Parity-off tape: Initialized and two Object barriers.

- bytes: 2033, SHA-256 f0961639687a2354d1d5a7c742d18c68df2b2e49c3ed199e0cb8acf74b0488b4
- header: tape UUID a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1, block size 262144, scheme none, 51 bytes
- tail: complete
- authoritative prefix: last closed sequence 8, closed end 2033
- counters: objects 2, next file 3, W 0, T 6, check 0000000000000000 (Empty), next epoch 0, cursor 0:10, sealed false

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 51 | 150 |
| 2 | SessionOpened (18) | 150 | 315 |
| 3 | PayloadContinuation (2) | 315 | 508 |
| 4 | PayloadContinuation (2) | 508 | 989 |
| 5 | Committed (3) | 989 | 1173 |
| 6 | PayloadContinuation (2) | 1173 | 1366 |
| 7 | PayloadContinuation (2) | 1366 | 1849 |
| 8 | Committed (3) | 1849 | 2033 |

## parity-object-session-close.remtape

Parity tape: one Object barrier leaving its epoch open, then ClosedOut and SessionEnded publishing the epoch's sidecar and clean end.

- bytes: 1688, SHA-256 1c2458bbe9c08a3510b62f73c9fc907bb402cfc444072498ba933cdaaa9765a5
- header: tape UUID a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2, block size 262144, scheme checkpoint-partial-epoch k=8 m=2 S=1, 84 bytes
- tail: complete
- authoritative prefix: last closed sequence 8, closed end 1688
- counters: objects 1, next file 3, W 3, T 3, check 0000000000000000 (Empty), next epoch 1, cursor 0:12, sealed false

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 84 | 216 |
| 2 | SessionOpened (18) | 216 | 381 |
| 3 | PayloadContinuation (2) | 381 | 574 |
| 4 | PayloadContinuation (2) | 574 | 1055 |
| 5 | Committed (3) | 1055 | 1239 |
| 6 | PayloadContinuation (2) | 1239 | 1345 |
| 7 | ClosedOut (17) | 1345 | 1530 |
| 8 | SessionEnded (19) | 1530 | 1688 |

## sealed-terminal.remtape

Sealed parity tape: an Object and sidecar barrier, an automatic FinalizationDecided plan, TerminalPrefix, five ComponentWritten records and Sealed.

- bytes: 3592, SHA-256 cde0bbd16dd187619f0f5b691ffd741d6e6b72fb90294484f4a74b48dbcce41b
- header: tape UUID a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3, block size 262144, scheme checkpoint-partial-epoch k=8 m=2 S=1, 84 bytes
- tail: complete
- authoritative prefix: last closed sequence 22, closed end 3592
- counters: objects 1, next file 9, W 3, T 3, check 0000000000000000 (Empty), next epoch 1, cursor 0:8220, sealed true

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 84 | 216 |
| 2 | SessionOpened (18) | 216 | 381 |
| 3 | PayloadContinuation (2) | 381 | 574 |
| 4 | PayloadContinuation (2) | 574 | 1055 |
| 5 | PayloadContinuation (2) | 1055 | 1161 |
| 6 | Committed (3) | 1161 | 1345 |
| 7 | PayloadContinuation (2) | 1345 | 1451 |
| 8 | PayloadContinuation (2) | 1451 | 1553 |
| 9 | FinalizationDecided (7) | 1553 | 1832 |
| 10 | PayloadContinuation (2) | 1832 | 1938 |
| 11 | TerminalPrefix (4) | 1938 | 2107 |
| 12 | PayloadContinuation (2) | 2107 | 2213 |
| 13 | ComponentWritten (5) | 2213 | 2383 |
| 14 | PayloadContinuation (2) | 2383 | 2491 |
| 15 | ComponentWritten (5) | 2491 | 2663 |
| 16 | PayloadContinuation (2) | 2663 | 2771 |
| 17 | ComponentWritten (5) | 2771 | 2943 |
| 18 | PayloadContinuation (2) | 2943 | 3053 |
| 19 | ComponentWritten (5) | 3053 | 3225 |
| 20 | PayloadContinuation (2) | 3225 | 3333 |
| 21 | ComponentWritten (5) | 3333 | 3505 |
| 22 | Sealed (6) | 3505 | 3592 |

## torn-tail.remtape

The parity-off tape with a third group whose closing frame is cut short by EOF. The first two groups remain authoritative.

- bytes: 2740, SHA-256 62705ef64e721ebe05b8edc7b1f2306254c75c2090dac879403de2fb14d29f12
- header: tape UUID a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1, block size 262144, scheme none, 51 bytes
- tail: torn: short_body at byte 2709
- authoritative prefix: last closed sequence 8, closed end 2033
- counters: objects 2, next file 3, W 0, T 6, check 0000000000000000 (Empty), next epoch 0, cursor 0:10, sealed false

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 51 | 150 |
| 2 | SessionOpened (18) | 150 | 315 |
| 3 | PayloadContinuation (2) | 315 | 508 |
| 4 | PayloadContinuation (2) | 508 | 989 |
| 5 | Committed (3) | 989 | 1173 |
| 6 | PayloadContinuation (2) | 1173 | 1366 |
| 7 | PayloadContinuation (2) | 1366 | 1849 |
| 8 | Committed (3) | 1849 | 2033 |
| 9 | PayloadContinuation (2) | 2033 | 2226 |
| 10 | PayloadContinuation (2) | 2226 | 2709 |

## zero-filled-tail.remtape

The two-Object parity-off log followed by 4096 zero bytes, as a filesystem may leave an unsynced append after power loss.

- bytes: 6129, SHA-256 ecf5d06153c634f517484d00dabcd203f5b6060b2fcf27656961a9ff97d3b2e8
- header: tape UUID a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1, block size 262144, scheme none, 51 bytes
- tail: torn: zero_filled at byte 2033
- authoritative prefix: last closed sequence 8, closed end 2033
- counters: objects 2, next file 3, W 0, T 6, check 0000000000000000 (Empty), next epoch 0, cursor 0:10, sealed false

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 51 | 150 |
| 2 | SessionOpened (18) | 150 | 315 |
| 3 | PayloadContinuation (2) | 315 | 508 |
| 4 | PayloadContinuation (2) | 508 | 989 |
| 5 | Committed (3) | 989 | 1173 |
| 6 | PayloadContinuation (2) | 1173 | 1366 |
| 7 | PayloadContinuation (2) | 1366 | 1849 |
| 8 | Committed (3) | 1849 | 2033 |

## recovery-required-tail.remtape

The two-Object parity-off log with one bit flipped in the last closing frame's structural section. Only the first group is authoritative.

- bytes: 2033, SHA-256 a97e3203d5a2578b792f3bbbef4a518528650f4759f496c0743d1d481063f407
- header: tape UUID a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1, block size 262144, scheme none, 51 bytes
- tail: recovery required at byte 1849, sequence 8: bad Structural CRC
- authoritative prefix: last closed sequence 5, closed end 1173
- counters: objects 1, next file 2, W 0, T 3, check 0000000000000000 (Empty), next epoch 0, cursor 0:6, sealed false

| sequence | tag | offset | end |
|---:|---|---:|---:|
| 1 | Initialized (1) | 51 | 150 |
| 2 | SessionOpened (18) | 150 | 315 |
| 3 | PayloadContinuation (2) | 315 | 508 |
| 4 | PayloadContinuation (2) | 508 | 989 |
| 5 | Committed (3) | 989 | 1173 |
| 6 | PayloadContinuation (2) | 1173 | 1366 |
| 7 | PayloadContinuation (2) | 1366 | 1849 |
