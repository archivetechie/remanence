# Library log v1 schema

Header: magic REMLIB\0\x01 (0..8), version u16 LE (8..10), log UUID b16 (10..26), first sequence u64 LE (26..34), previous hash b32 (34..66), valid ASCII date (66..76), CRC-64/XZ (76..84).

Frame: prefix version:u16/tag:u16/structural_len:u32/payload_len:u32/sequence:u64, all LE; prefix CRC (20..28); structural CBOR; structural CRC; 32-byte record hash; payload CRC. Overhead 44 bytes, combined cap 64 MiB. The payload is the 32-byte record hash, so a valid frame has payload_len 32; the order of that check is given below. `FrameTooLarge` ("frame exceeds 64 MiB") is raised when `structural_len + payload_len` exceeds 64 MiB, before reading the body. For a complete 28-byte prefix the check order is prefix CRC, version, tag, sequence, then the cap; a cap refusal includes the decoded sequence. An EOF-short prefix instead checks the visible identity, then the cap, then the visible sequence, with no CRC check and no reported sequence; the exact tail rules follow below.

record_hash = SHA-256(previous_hash || prefix20 || structural). First previous hash is zero. No CRC or hash covers itself. Structural map is {0: envelope, 1: body}. Structural CBOR is validated before the record hash, so malformed structure with a stale hash is `NoncanonicalCbor`. All keys and fields below are mandatory, with null for absent Option. TextValue/Text max 4096 UTF-8 bytes; LabelText max 32; Blob<N> and Bounded<T,N> enforce N before allocation. Pure enums are unsigned; unions are [code,payload], including null unit payloads.

| Header offset | Bytes | Field |
|---:|---:|---|
| 0 | 8 | REMLIB magic |
| 8 | 2 | Version 1 |
| 10 | 16 | Log UUID |
| 26 | 8 | First sequence |
| 34 | 32 | Previous segment hash |
| 66 | 10 | ASCII date |
| 76 | 8 | CRC of bytes 0..76 |

## Segments

Names are `<first_sequence:020>-<YYYY-MM-DD>.remlib`, using the header date. Discovery selects every directory entry whose extension is exactly `remlib`, without a regular-file check; other extensions, including `.REMLIB` and `.remlib.bak`, are ignored. A selected name differing from its decoded header name is `SegmentName`, never silently skipped; header and I/O failures can precede this check.

Segments are read in lexical name order, which is first-sequence order for well-formed names. Each first sequence is the previous last sequence plus one (`SegmentSequence`), starting at 1; its previous hash is the previous segment's last record hash, zero for the first (`PreviousSegmentHash`). After header validation, the per-segment checks run name, then sequence, then previous hash; the first refusal wins.

Dates are diagnostic for replay ordering. Rotation occurs when the UTC date advances beyond the active segment's date; after clock regression the store keeps appending to the active segment.

Rotation publishes the header and first frame together. If there is no prior refusal, a segment whose authoritative end is the 84-byte header is `EmptySegment` at offset 84 with sequence last plus one, checked before `InteriorTorn`. This includes a header followed only by a torn first frame: `EmptySegment` wins over the torn-tail classification.

At a frame boundary, zero bytes read means a complete tail. If all bytes read into the prefix (up to 28) are zero, scan the suffix: a later valid prefix at any byte offset (valid prefix CRC, version, tag and lengths within the cap; sequence need not match) gives a prefix CRC refusal; otherwise a later nonzero byte gives `NonZeroAfterZeros` at that byte; otherwise the tail is `ZeroFilled`. These refusals have no sequence.

For a nonzero EOF-short prefix (1..27 bytes), a zero first byte gives `NonZeroAfterZeros` at its first nonzero byte, before any identity check. Otherwise check the present version bytes against version 1, then the tag if any tag byte is present (zero-fill its missing high byte). Next zero-fill missing length bytes and check the resulting lower-bound `structural_len + payload_len` against 64 MiB: an excess is `FrameTooLarge`, even though the prefix is short. Only then compare any present sequence bytes with the expected sequence's LE bytes. Every refusal on this path has sequence None; no prefix CRC is checked. Only a short prefix that passes these identity, cap and sequence checks is an absent frame with a `ShortPrefix` torn tail.

For a nonzero complete prefix, a failed prefix CRC is refused with sequence None, except that a zero first byte and no later valid prefix at any byte offset gives `NonZeroAfterZeros` at the first nonzero prefix byte. After a valid prefix CRC, check version, tag, sequence and cap in that order; these refusals include the decoded sequence. Only then read the body. Check any fully present structural CRC, then the payload CRC if the body is complete. A complete body whose CRCs pass is then decoded as canonical structural CBOR; after that the payload length is checked (`PayloadLength` unless the payload is exactly 32 bytes), and only then the record hash. `PayloadLength` is never checked on a short or damaged body: such a body goes to the damaged-body classifier below, and its refusals carry the sequence decoded from the frame's valid prefix, `NonZeroAfterZeros` on this path included. For a short body or failed body CRC, scan the unread suffix after the body bytes read, then apply these checks in order: a later valid prefix at any byte offset is a CRC refusal (the failed section, or Structural when no CRC failed); a complete damaged body starting with zero but containing nonzero bytes is `NonZeroAfterZeros`; a qualifying trailing zero run is `ZeroFilled` unless a later nonzero byte makes it `NonZeroAfterZeros`; otherwise a failed CRC is refused; otherwise an EOF-short body is `ShortBody`. A trailing zero run qualifies exactly when it is nonempty and starts at or before S, or when it is nonempty, P > 0, more than S+8 body bytes were read, and it starts before S+8+P; positions are measured from the body start. A structural CRC failure takes precedence over a payload CRC failure. Valid complete bodies bypass this damaged-body classifier.

In the last nonempty segment, a frame classified `ShortBody`, `ShortPrefix` or `ZeroFilled` is absent and the tail is torn. The same torn tail in an earlier nonempty segment is `InteriorTorn`; the empty-segment rule above takes precedence. A short header is `Header`.

| Frame offset | Bytes | Field |
|---:|---:|---|
| 0 | 2 | Version 1 |
| 2 | 2 | Tag |
| 4 | 4 | Structural length S |
| 8 | 4 | Payload length 32 |
| 12 | 8 | Sequence |
| 20 | 8 | Prefix CRC |
| 28 | S | Structural CBOR |
| 28+S | 8 | Structural CRC |
| 36+S | 32 | Record hash |
| 68+S | 8 | Payload CRC |

AuditActor is map {0: kind uint (0 System, 1 User, 2 Service), 1: name text4096|null}; name is null exactly for System. Numeric Rust scalars map to CBOR unsigned integers with their declared bit width; Id is b16, Hash b32, Text text4096, LabelText text32. Pool watermarks preserve f64 bits as unsigned u64. Nested named types are maps in the tables below.

## ParityScheme

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | id | `Text` | false |
| 1 | data_blocks_per_stripe | `u16` | false |
| 2 | parity_blocks_per_stripe | `u16` | false |
| 3 | stripes_per_neighborhood | `u32` | false |

## Envelope

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | record_uuid | `Id` | false |
| 1 | timestamp_unix_ns | `u64` | false |
| 2 | host_id | `Text` | false |
| 3 | process_id | `u32` | false |
| 4 | software_build | `Text` | false |
| 5 | actor | `AuditActor` | false |
| 6 | source_layer | `SourceLayer` | false |
| 7 | operation_id | `Option<Id>` | true |
| 8 | session_id | `Option<Id>` | true |
| 9 | idempotency_key | `Option<Id>` | true |

## TapeRef

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | uuid | `Id` | false |
| 1 | incarnation | `u64` | false |

## TapeAt

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | tape | `TapeRef` | false |
| 1 | sequence | `u64` | false |
| 2 | record_digest | `Hash` | false |

`TapeAt.record_digest` is SHA-256 over the referenced tape-log frame's 20 prefix bytes, structural bytes and payload bytes, without any CRC.

## DecisionRef

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | sequence | `u64` | false |
| 1 | record_uuid | `Id` | false |

## GenerationChange

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | expected | `u64` | false |
| 1 | next | `u64` | false |

## CompletionEvidence

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | dispatch | `Dispatch` | false |
| 1 | completion | `Completion` | false |
| 2 | scsi_status | `Option<u8>` | true |
| 3 | host_status | `Option<u16>` | true |
| 4 | driver_status | `Option<u16>` | true |
| 5 | sense | `Option<Blob<252>>` | true |
| 6 | residual | `Option<u64>` | true |

## Bay

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | library_serial | `Text` | false |
| 1 | bay | `u16` | false |

## Standalone

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | drive_serial | `Text` | false |

## PriorHolder

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | tape | `TapeRef` | false |
| 1 | change | `GenerationChange` | false |

## MediumSpec

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | family | `MediumFamily` | false |
| 1 | usable_logical_bytes | `u64` | false |
| 2 | record_size | `u32` | false |
| 3 | rerecord | `bool` | false |
| 4 | lifecycle_ops | `bool` | false |
| 5 | source | `GenerationSource` | false |

## ModelScope

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | model | `Text` | false |
| 1 | firmware | `Text` | false |

## Adoption

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | request_fingerprint | `Hash` | false |
| 1 | operation | `Id` | false |
| 2 | idempotency_key | `Id` | false |
| 3 | physical_tail | `PhysicalTail` | false |
| 4 | state | `AdoptedState` | false |
| 5 | library_revision | `Option<Text>` | true |
| 6 | home_slot | `Option<u16>` | true |
| 7 | drive_serial | `Text` | false |
| 8 | identity_only | `bool` | false |
| 9 | bootstrap_drive_compression | `bool` | false |
| 10 | configured_drive_compression | `bool` | false |

## Load

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | endpoint | `DriveKey` | false |
| 1 | attempt | `u32` | false |

## Unload

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | endpoint | `DriveKey` | false |

## Move

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | library_serial | `Text` | false |
| 1 | source | `u16` | false |
| 2 | destination | `u16` | false |

## Element

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | address | `u16` | false |
| 1 | kind | `ElementKind` | false |
| 2 | barcode | `Option<LabelText>` | true |
| 3 | full | `bool` | false |

## VerificationCursor

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | partition | `u16` | false |
| 1 | file | `u64` | false |
| 2 | lba | `u64` | false |
| 3 | ordinal | `u64` | false |
| 4 | part | `u32` | false |

## DamagedRange

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | partition | `u16` | false |
| 1 | start_lba | `u64` | false |
| 2 | end_lba | `u64` | false |

## Unprotected

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | w | `u64` | false |
| 1 | t | `u64` | false |

## PoolDefinition

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | id | `Text` | false |
| 1 | display_name | `Option<Text>` | true |
| 2 | copy_class | `Option<Text>` | true |
| 3 | content_class | `Option<Text>` | true |
| 4 | scheme | `Option<Text>` | true |
| 5 | selection_policy | `SelectionPolicy` | false |
| 6 | watermark_low_bits | `u64` | false |
| 7 | watermark_high_bits | `u64` | false |
| 8 | capacity_cap_bytes | `Option<u64>` | true |
| 9 | block_size_bytes | `u64` | false |
| 10 | min_object_size_bytes | `u64` | false |

## PoolRule

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | prefix | `Text` | false |
| 1 | pool_id | `Text` | false |

## Definitions

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | pools | `Bounded<PoolDefinition, 4096>` | false |
| 1 | rules | `Bounded<PoolRule, 4096>` | false |
| 2 | cleaning_prefixes | `Bounded<Text, 256>` | false |

## Reason

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | reason | `Text` | false |

## Annotation

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | purchase_date | `Option<Text>` | true |
| 1 | warranty_until | `Option<Text>` | true |
| 2 | cost | `Option<Text>` | true |
| 3 | notes | `Option<Text>` | true |

## Counters

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | write_corrected | `Option<u64>` | true |
| 1 | write_uncorrected | `Option<u64>` | true |
| 2 | read_corrected | `Option<u64>` | true |
| 3 | read_uncorrected | `Option<u64>` | true |

## Register

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | barcode | `LabelText` | false |
| 1 | home_slot | `u16` | false |

## Begin

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | trigger | `CleanTrigger` | false |

## Advance

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | phase | `CleanPhase` | false |
| 1 | home_slot | `Option<u16>` | true |

## Finish

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | result | `CleanResult` | false |
| 1 | verified_credit | `bool` | false |

## AlarmRefresh

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | severity | `Severity` | false |
| 1 | reason | `Text` | false |
| 2 | cause | `Option<DecisionRef>` | true |

## AlarmRaise

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | kind | `AlarmKind` | false |
| 1 | severity | `Severity` | false |
| 2 | reason | `Text` | false |

## Accepted

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | actor_fingerprint | `Text` | false |
| 1 | idempotency_key | `Option<Id>` | true |
| 2 | request_fingerprint | `Hash` | false |
| 3 | subject | `Option<Text>` | true |

## Response

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | response_fingerprint | `Option<Hash>` | true |

## Failed

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | retry | `Option<Retry>` | true |
| 1 | refusal_code | `Option<u32>` | true |
| 2 | summary | `Text` | false |

RefusalCode (also RefusalRecorded.detail code): zero is never emitted. The fold does not yet check `Failed.refusal_code` against this table.

| Code | Name |
|---:|---|
| 1 | TapeLogBehindProjection |
| 2 | ContradictedTapeAdmission |
| 3 | ClosedOperationRetry |
| 4 | RetainedIntentForeignOwner |
| 5 | StandaloneFenceUnrecorded |
| 6 | ReconcileTapeDeferred |
| 7 | UnknownGenerationAdoption |
| 8 | DriveIdentityConflictLatch |
| 9 | ForeignDrivePollDisabled |
| 10 | DriveStewardshipDeferred |
| 11 | CheckpointBarrierAlarmUnrecorded |
| 12 | DriveFenced |
| 13 | TailRepairDeferred |
| 14 | JournalImportDeferred |

## Cache

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | descriptors | `Blob<4194304>` | false |
| 1 | mapped_extent_lba | `u64` | false |
| 2 | write_epoch | `u64` | false |
| 3 | digest | `Hash` | false |

## RepairApplied

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | archived_prefix_digest | `Hash` | false |
| 1 | retained_through | `u64` | false |

## Recovered

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | source_digest | `Hash` | false |

## AssignPool

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | tape | `TapeRef` | false |
| 1 | pool | `Option<Text>` | true |
| 2 | change | `GenerationChange` | false |
| 3 | source | `AssignmentSource` | false |
| 4 | configuration | `Option<DecisionRef>` | true |

## Label

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | tape | `TapeRef` | false |
| 1 | label | `Option<LabelText>` | true |
| 2 | provenance | `LabelProvenance` | false |
| 3 | change | `GenerationChange` | false |
| 4 | prior_holder | `Option<PriorHolder>` | true |
| 5 | observed_bot_uuid | `Option<Id>` | true |

## Retire

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | tape | `TapeRef` | false |
| 1 | reason | `RetireReason` | false |
| 2 | observed_tape_sequence | `Option<u64>` | true |
| 3 | assignment_generation | `u64` | false |
| 4 | detail | `Option<Text>` | true |

## Reprovision

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | tape | `TapeRef` | false |
| 1 | predecessor | `Option<TapeRef>` | true |
| 2 | mode | `ProvisionMode` | false |
| 3 | label | `Option<LabelText>` | true |
| 4 | label_provenance | `Option<LabelProvenance>` | true |
| 5 | parity_scheme | `Option<ParityScheme>` | true |
| 6 | medium | `MediumSpec` | false |
| 7 | pool | `Option<Text>` | true |
| 8 | endpoint | `Option<DriveKey>` | true |
| 9 | adoption | `Option<Adoption>` | true |
| 10 | pool_source | `Option<PoolSource>` | true |
| 11 | operation | `Option<Id>` | true |
| 12 | replaced_bot | `Option<Id>` | true |

## MediumSpecSet

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | tape | `TapeRef` | false |
| 1 | medium | `MediumSpec` | false |
| 2 | change | `GenerationChange` | false |

## SupersedeTail

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | at | `TapeAt` | false |
| 1 | operation | `Option<Id>` | true |
| 2 | session_id | `Option<Id>` | true |
| 3 | preview_digest | `Option<Hash>` | true |
| 4 | tail_file_count | `u64` | false |

## AcceptUnprotectedFinalization

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | at | `TapeAt` | false |
| 1 | operation | `Id` | false |
| 2 | w | `u64` | false |
| 3 | t | `u64` | false |
| 4 | reason | `Text` | false |

## FenceRaised

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | kind | `FenceKind` | false |
| 1 | scope | `FenceScope` | false |
| 2 | cause | `FenceCause` | false |
| 3 | evidence | `Option<CompletionEvidence>` | true |
| 4 | at | `Option<TapeAt>` | true |
| 5 | operation | `Option<Id>` | true |
| 6 | detail | `Bounded<DiagnosticEntry, 256>` | false |

## FenceReleased

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | fence | `u64` | false |
| 1 | evidence | `Text` | false |
| 2 | forced | `bool` | false |
| 3 | detail | `Bounded<DiagnosticEntry, 256>` | false |

## MotionIntent

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | command | `Id` | false |
| 1 | parent_operation | `Option<Id>` | true |
| 2 | motion | `Motion` | false |
| 3 | tape | `Option<TapeRef>` | true |
| 4 | inventory_generation | `Option<u64>` | true |
| 5 | readiness_generation | `Option<u64>` | true |

## MotionOutcome

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | intent | `DecisionRef` | false |
| 1 | result | `MotionResult` | false |
| 2 | evidence | `CompletionEvidence` | false |
| 3 | confirmation | `Option<Confirmation>` | true |

## InventoryReconciled

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | subject | `InventorySubject` | false |
| 1 | observation_generation | `u64` | false |
| 2 | resolves | `Bounded<DecisionRef, 1024>` | false |
| 3 | complete | `bool` | false |
| 4 | elements | `Bounded<Element, 65535>` | false |

## VerificationProgress

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | operation | `Id` | false |
| 1 | at | `TapeAt` | false |
| 2 | plan_digest | `Hash` | false |
| 3 | modes | `u32` | false |
| 4 | parts_done | `u32` | false |
| 5 | next | `VerificationCursor` | false |
| 6 | completed_digest | `Hash` | false |

## VerificationVerdict

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | operation | `Id` | false |
| 1 | at | `Option<TapeAt>` | true |
| 2 | damaged | `Bounded<DamagedRange, 4096>` | false |
| 3 | unprotected | `Option<Unprotected>` | true |
| 4 | modes | `u32` | false |
| 5 | edition_digest | `Option<Hash>` | true |
| 6 | progress | `Option<DecisionRef>` | true |
| 7 | tape | `TapeRef` | false |
| 8 | outcome | `VerificationOutcome` | false |

## ManualFinalizationAccepted

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | at | `TapeAt` | false |
| 1 | operation | `Id` | false |
| 2 | actor_fingerprint | `Text` | false |
| 3 | idempotency_key | `Id` | false |
| 4 | request_fingerprint | `Hash` | false |
| 5 | assignment_generation | `u64` | false |

## ConfigurationAccepted

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | generation | `u64` | false |
| 1 | config_digest | `Hash` | false |
| 2 | definitions | `Definitions` | false |

## ReadinessObserved

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | operation | `Id` | false |
| 1 | endpoint | `DriveKey` | false |
| 2 | tape | `Option<TapeRef>` | true |
| 3 | barcode | `Option<LabelText>` | true |
| 4 | phase | `ReadinessPhase` | false |
| 5 | state | `ReadinessState` | false |
| 6 | previous | `Option<DecisionRef>` | true |
| 7 | started_unix_ns | `u64` | false |
| 8 | limit_ms | `Option<u64>` | true |
| 9 | evidence | `Option<CompletionEvidence>` | true |
| 10 | load_intent | `Option<DecisionRef>` | true |
| 11 | ready_generation | `u64` | false |
| 12 | result | `ReadinessResult` | false |
| 13 | scope | `ReadinessScope` | false |
| 14 | media | `Option<MediumFamily>` | true |
| 15 | source_slot | `Option<u16>` | true |
| 16 | run | `Option<Id>` | true |
| 17 | cancel_source | `Option<CancelSource>` | true |

## DriveObserved

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | drive_uuid | `Id` | false |
| 1 | serial | `Option<Text>` | true |
| 2 | identity_source | `IdentitySource` | false |
| 3 | endpoint | `Option<DriveKey>` | true |
| 4 | managed | `Managed` | false |
| 5 | inventory_generation | `u64` | false |
| 6 | vendor | `Option<Text>` | true |
| 7 | product | `Option<Text>` | true |
| 8 | firmware_rev | `Option<Text>` | true |

## DriveDecision

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | drive_uuid | `Id` | false |
| 1 | decision | `Stewardship` | false |
| 2 | change | `GenerationChange` | false |

## DriveHealthObserved

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | drive_uuid | `Id` | false |
| 1 | tape | `Option<TapeRef>` | true |
| 2 | session_id | `Option<Id>` | true |
| 3 | operation | `Option<Id>` | true |
| 4 | trigger | `HealthTrigger` | false |
| 5 | tape_alert_flags | `u64` | false |
| 6 | counters | `Counters` | false |
| 7 | raw_pages | `Option<Blob<65536>>` | true |

## CleaningRecorded

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | run_id | `Text` | false |
| 1 | previous | `Option<DecisionRef>` | true |
| 2 | drive_uuid | `Option<Id>` | true |
| 3 | cartridge | `Option<TapeRef>` | true |
| 4 | action | `CleaningAction` | false |

## AlarmChanged

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | condition_key | `Text` | false |
| 1 | change | `GenerationChange` | false |
| 2 | action | `AlarmAction` | false |
| 3 | cause | `Option<DecisionRef>` | true |

## OperationRecorded

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | operation | `Id` | false |
| 1 | kind | `OperationKind` | false |
| 2 | previous | `Option<DecisionRef>` | true |
| 3 | event | `OperationEvent` | false |

## SessionObserved

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | session_id | `Id` | false |
| 1 | kind | `SessionKind` | false |
| 2 | endpoint | `Option<DriveKey>` | true |
| 3 | tape | `Option<TapeRef>` | true |
| 4 | event | `SessionEvent` | false |
| 5 | previous | `Option<DecisionRef>` | true |
| 6 | reason | `Option<Text>` | true |

## WrapMapCached

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | tape | `TapeRef` | false |
| 1 | calibration_generation | `u64` | false |
| 2 | action | `WrapAction` | false |

## RecoveryRecorded

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | at | `TapeAt` | false |
| 1 | action | `RecoveryAction` | false |
| 2 | operation | `Id` | false |
| 3 | prior | `Option<DecisionRef>` | true |

## Diagnostic

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | event | `DiagnosticEvent` | false |
| 1 | subject_kind | `Text` | false |
| 2 | subject_id | `Option<Text>` | true |
| 3 | detail | `Bounded<DiagnosticEntry, 256>` | false |

## Integer and union tables

### SourceLayer

| Code | Variant / payload |
|---:|---|
| 0 | `Layer2` |
| 1 | `Layer3b` |
| 2 | `Layer3c` |
| 3 | `Layer4` |
| 4 | `Layer5` |

### LtoGen

| Code | Variant / payload |
|---:|---|
| 0 | `Lto1` |
| 1 | `Lto2` |
| 2 | `Lto3` |
| 3 | `Lto4` |
| 4 | `Lto5` |
| 5 | `Lto6` |
| 6 | `Lto7` |
| 7 | `M8` |
| 8 | `Lto8` |
| 9 | `Lto9` |

### GenerationSource

| Code | Variant / payload |
|---:|---|
| 0 | `Measured` |
| 1 | `Label` |
| 2 | `Operator` |

### AssignmentSource

| Code | Variant / payload |
|---:|---|
| 0 | `Operator` |
| 1 | `Rule` |

### LabelProvenance

| Code | Variant / payload |
|---:|---|
| 0 | `Changer` |
| 1 | `Operator` |

### RetireReason

| Code | Variant / payload |
|---:|---|
| 0 | `OperatorRequested` |

### ProvisionMode

| Code | Variant / payload |
|---:|---|
| 0 | `Initialize` |
| 1 | `Adopt` |

### Dispatch

| Code | Variant / payload |
|---:|---|
| 0 | `NotIssued` |
| 1 | `Issued` |

### Completion

| Code | Variant / payload |
|---:|---|
| 0 | `Known` |
| 1 | `Unknown` |

### FenceKind

| Code | Variant / payload |
|---:|---|
| 0 | `Write` |
| 1 | `DriveFault` |

### FenceCause

| Code | Variant / payload |
|---:|---|
| 0 | `PartialBatch` |
| 1 | `TransferError` |
| 2 | `CloseoutCheckMismatch` |
| 3 | `SupersedeTailFailed` |
| 4 | `StandaloneWriteLostByRestart` |
| 5 | `AppendAdmissionRefused` |
| 6 | `ResetUnitAttention` |
| 7 | `PositionDrift` |
| 8 | `AuthorityAmbiguous` |
| 9 | `SessionSuffixContradiction` |
| 10 | `CloseoutReadContradiction` |
| 11 | `Cleaning` |
| 12 | `ContainerQuarantine` |
| 13 | `CheckpointBarrierFailed` |
| 14 | `NativeLogBehindProjection` |

### MotionResult

| Code | Variant / payload |
|---:|---|
| 0 | `Done` |
| 1 | `Failed` |
| 2 | `Unknown` |

### ElementKind

| Code | Variant / payload |
|---:|---|
| 0 | `Transport` |
| 1 | `Storage` |
| 2 | `ImportExport` |
| 3 | `Drive` |

### SelectionPolicy

| Code | Variant / payload |
|---:|---|
| 0 | `CompleteOrFill` |
| 1 | `FillOldest` |

### PhysicalTail

| Code | Variant / payload |
|---:|---|
| 0 | `ExactBootstrapFilemarkEod` |
| 1 | `DataAfterBootstrap` |
| 2 | `MissingFilemark` |
| 3 | `ExtraFilemark` |
| 4 | `Ambiguous` |

### AdoptedState

| Code | Variant / payload |
|---:|---|
| 0 | `Ready` |
| 1 | `RecoveryRequired` |

### ReadinessPhase

| Code | Variant / payload |
|---:|---|
| 0 | `Planned` |
| 1 | `InitializingCommandLoad` |
| 2 | `ReadinessPoll` |
| 3 | `Ready` |
| 4 | `Cancelled` |
| 5 | `Shutdown` |
| 6 | `SessionOpenShortProbe` |
| 7 | `SessionOpenImmediateLoad` |
| 8 | `SessionOpenAfterImmediateLoad` |
| 9 | `StartupReconcileTur` |
| 10 | `StartupReconcile` |
| 11 | `StartupReconcileRequiresRelease` |
| 12 | `QuarantineRelease` |
| 13 | `Released` |
| 14 | `WaitReadyTur` |
| 15 | `AfterWaitReadyTur` |
| 16 | `BeforeConditionalImmediateLoad` |
| 17 | `ConditionalImmediateLoad` |
| 18 | `AfterConditionalImmediateLoad` |
| 19 | `AfterBotClassification` |
| 20 | `AfterClobberConfirmation` |
| 21 | `AfterMoveMedium` |
| 22 | `AfterOpenDrive` |
| 23 | `AfterOpenLibrary` |
| 24 | `AfterReadMediumPartitions` |
| 25 | `AfterRewindBeforeBotRead` |
| 26 | `ApplyInitWriteGate` |
| 27 | `BeforeApplyInitWriteGate` |
| 28 | `BeforeMoveMedium` |
| 29 | `BeforeOpenDrive` |
| 30 | `BeforeOpenLibrary` |
| 31 | `BeforeReadDriveConfig` |
| 32 | `BeforeRewindBeforeBootstrapWrite` |
| 33 | `BeforeRewindBeforeBotRead` |
| 34 | `BeforeWaitReadyTur` |
| 35 | `MoveMedium` |
| 36 | `OpenDrive` |
| 37 | `OpenLibrary` |
| 38 | `ReadDriveConfig` |
| 39 | `ReadMediumPartitions` |
| 40 | `RewindBeforeBootstrapWrite` |
| 41 | `RewindBeforeBotRead` |
| 42 | `ClobberConfirmation` |
| 43 | `PreLoadTur` |
| 44 | `AlreadyLoadedTur` |
| 45 | `BeforePreLoadTur` |
| 46 | `AfterPreLoadTur` |
| 47 | `BeforeAlreadyLoadedTur` |
| 48 | `AfterAlreadyLoadedTur` |
| 49 | `LoadDriveReadiness` |
| 50 | `LoadDriveCompletion` |

### ReadinessState

| Code | Variant / payload |
|---:|---|
| 0 | `Planned` |
| 1 | `PreReadyLoading` |
| 2 | `Ready` |
| 3 | `MediaInitializing` |
| 4 | `BecomingReady` |
| 5 | `UnitAttention` |
| 6 | `TargetBusy` |
| 7 | `ReservationConflict` |
| 8 | `TransportUnknown` |
| 9 | `TerminalError` |
| 10 | `TimeoutUnknown` |
| 11 | `AbortedUnknown` |
| 12 | `Cancelled` |
| 13 | `Released` |
| 14 | `Starting` |
| 15 | `RecordingFailed` |

### ReadinessScope

| Code | Variant / payload |
|---:|---|
| 0 | `Endpoint` |
| 1 | `Library` |

### ReadinessResult

| Code | Variant / payload |
|---:|---|
| 0 | `Pending` |
| 1 | `Ready` |
| 2 | `Failed` |
| 3 | `Cancelled` |
| 4 | `AbortedUnknown` |
| 5 | `Released` |

### IdentitySource

| Code | Variant / payload |
|---:|---|
| 0 | `Standalone` |
| 1 | `DvcidInline` |
| 2 | `DvcidAndInquiry` |
| 3 | `Derived` |

### Managed

| Code | Variant / payload |
|---:|---|
| 0 | `Rem` |
| 1 | `Foreign` |

### HealthTrigger

| Code | Variant / payload |
|---:|---|
| 0 | `SessionClose` |
| 1 | `AppendFailure` |
| 2 | `ReadFailure` |
| 3 | `ForeignCounter` |
| 4 | `Manual` |

### CleanTrigger

| Code | Variant / payload |
|---:|---|
| 0 | `Manual` |
| 1 | `Periodic` |
| 2 | `Now` |

### CleanPhase

| Code | Variant / payload |
|---:|---|
| 0 | `Fencing` |
| 1 | `Selecting` |
| 2 | `MovingIn` |
| 3 | `Cleaning` |
| 4 | `MovingBack` |
| 5 | `Verifying` |
| 6 | `Done` |
| 7 | `Failed` |
| 8 | `NeedsOperator` |

### CleanResult

| Code | Variant / payload |
|---:|---|
| 0 | `Done` |
| 1 | `Failed` |
| 2 | `NeedsOperator` |

### AlarmKind

| Code | Variant / payload |
|---:|---|
| 0 | `DriveCleaningAbnormalFrequency` |
| 1 | `CleaningNeedsOperator` |
| 2 | `NoClnCart` |
| 3 | `ClnCartExpired` |
| 4 | `CartNotCleaningBehavior` |
| 5 | `SnapshotPersistFailing` |
| 6 | `CheckpointBarrierOverdue` |
| 7 | `ForeignDriveWantsCleaning` |
| 8 | `KindFlipRefused` |
| 9 | `ContradictedTape` |
| 10 | `PendingRecordRejected` |
| 11 | `DriveSerialCollision` |
| 12 | `RetiredDriveReappeared` |

### Severity

| Code | Variant / payload |
|---:|---|
| 0 | `Warning` |
| 1 | `Critical` |
| 2 | `Error` |

### OperationKind

| Code | Variant / payload |
|---:|---|
| 0 | `Unknown` |
| 1 | `AckAlarm` |
| 2 | `DriveStewardship` |
| 3 | `CleanDrive` |
| 4 | `RefreshInventory` |
| 5 | `MoveMedium` |
| 6 | `LoadDrive` |
| 7 | `UnloadDrive` |
| 8 | `MediaReadinessWait` |
| 9 | `TapeVerification` |
| 10 | `FinalizeTape` |
| 11 | `ReconcileTape` |
| 12 | `TapeCloseout` |
| 13 | `CloseoutOverdue` |

### Retry

| Code | Variant / payload |
|---:|---|
| 0 | `Transient` |
| 1 | `Operator` |
| 2 | `Media` |

### SessionKind

| Code | Variant / payload |
|---:|---|
| 1 | `Read` |
| 2 | `Operation` |

### SessionEvent

| Code | Variant / payload |
|---:|---|
| 0 | `Opened` |
| 1 | `Closed` |
| 2 | `Orphaned` |
| 3 | `LostByRestart` |

### DiagnosticEvent

| Code | Variant / payload |
|---:|---|
| 0 | `RequestReceived` |
| 1 | `OperationStarted` |
| 2 | `OperationProgress` |
| 3 | `OperationFinished` |
| 4 | `OperationFailed` |
| 5 | `CancelRequested` |
| 6 | `CancelledBeforeDispatch` |
| 7 | `CompletedAfterCancel` |
| 8 | `CancellationRejected` |
| 9 | `CompletionUnknown` |
| 10 | `ClockRegressionObserved` |
| 11 | `ClockForwardJumpObserved` |
| 12 | `HardwareWarning` |
| 13 | `RecoveryEvent` |
| 14 | `ConfigLoaded` |
| 15 | `ConfigRejected` |
| 16 | `IndexRebuilt` |
| 17 | `ReadOnlyModeEntered` |
| 18 | `ReadOnlyModeLeft` |
| 19 | `AuditWriteFailed` |
| 20 | `RefusalRecorded` |

### CancelSource

| Code | Variant / payload |
|---:|---|
| 0 | `Operator` |
| 1 | `Shutdown` |
| 2 | `StartupReconcile` |

### VerificationOutcome

| Code | Variant / payload |
|---:|---|
| 0 | `VerifiedComplete` |
| 1 | `VerifiedDegraded` |
| 2 | `RecoveryRequired` |

### PoolSource

| Code | Variant / payload |
|---:|---|
| 0 | `Operator(Unit)` |
| 1 | `Rule(DecisionRef)` |

### DriveKey

| Code | Variant / payload |
|---:|---|
| 0 | `Bay(Bay)` |
| 1 | `Standalone(Standalone)` |

### Motion

| Code | Variant / payload |
|---:|---|
| 0 | `Load(Load)` |
| 1 | `Unload(Unload)` |
| 2 | `Move(Move)` |

### InventorySubject

| Code | Variant / payload |
|---:|---|
| 0 | `Library(Text)` |
| 1 | `Standalone(Standalone)` |

### Confirmation

| Code | Variant / payload |
|---:|---|
| 0 | `Inventory(u64)` |
| 1 | `Readiness(u64)` |

### Stewardship

| Code | Variant / payload |
|---:|---|
| 0 | `Retire(Reason)` |
| 1 | `Reinstate(Reason)` |
| 2 | `Annotate(Annotation)` |

### CleaningAction

| Code | Variant / payload |
|---:|---|
| 0 | `Register(Register)` |
| 1 | `Begin(Begin)` |
| 2 | `Advance(Advance)` |
| 3 | `Finish(Finish)` |
| 4 | `Expire(Reason)` |
| 5 | `Reject(Reason)` |

### AlarmAction

| Code | Variant / payload |
|---:|---|
| 0 | `Raise(AlarmRaise)` |
| 1 | `Acknowledge(Unit)` |
| 2 | `Clear(Reason)` |
| 3 | `Refresh(AlarmRefresh)` |

### OperationEvent

| Code | Variant / payload |
|---:|---|
| 0 | `Accepted(Accepted)` |
| 1 | `Started(Unit)` |
| 2 | `CancelRequested(Unit)` |
| 3 | `CancellationRejected(Unit)` |
| 4 | `CancelledBeforeDispatch(Unit)` |
| 5 | `CompletedAfterCancel(Response)` |
| 6 | `Finished(Response)` |
| 7 | `Failed(Failed)` |
| 8 | `CompletionUnknown(Unit)` |

### WrapAction

| Code | Variant / payload |
|---:|---|
| 0 | `Cache(Cache)` |
| 1 | `Invalidate(Reason)` |

### RecoveryAction

| Code | Variant / payload |
|---:|---|
| 1 | `RepairApplied(RepairApplied)` |
| 2 | `AppendAuthorityRecovered(Recovered)` |

### DiagnosticValue

| Code | Variant / payload |
|---:|---|
| 0 | `Text(Text)` |
| 1 | `Unsigned(u64)` |
| 2 | `Negative(Negative)` |
| 3 | `Bytes(Blob<4194304>)` |
| 4 | `Bool(bool)` |
| 5 | `Null(Unit)` |

### MediumFamily

| Code | Variant / payload |
|---:|---|
| 0 | Lto(LtoGen) |
| 1 | Disk(null) |

### FenceScope

| Code | Variant / payload |
|---:|---|
| 0 | Tape(TapeRef) |
| 1 | Drive(DriveKey) |
| 2 | Model(ModelScope), reserved |

MediumSpec stores capabilities, never role. rerecord implies regenerative; lifecycle_ops gates lifecycle operations independently of family. Source Measured on a revision attests a fresh measurement, not inherited provenance. Zero capacity is permitted (no usable space).

## Replay and pending

Tags 6, 7, 13, 15 and 26, and tag 14 only when at is Some, depend on TapeAt; tag 15 requires native FinalizationPrepared. Present has both kind and digest. NotYet queues all touched scopes in source order, except FenceRaised always applies. Replay receives every frame once, in sequence order, and never retries a pending record. Pending records are retried (`resolve`, to a fixed point) only before a live append, under the view current at that append. Except for deferred-fence liveness failures described in tag 8, if retrying a pending record returns a library FoldError (rather than remaining Unresolved or becoming a scoped tape contradiction), resolution stops at that error, retains the failing record and the unvisited remainder as pending, and keeps earlier progress from that resolution. The live append propagates that error before constructing or writing its new frame. Later appends retry the same pending record and fail again while that error persists. Byte faults and library FoldError stop at the previous authoritative prefix. TapeContradiction isolates its tape and continues. Contradiction status alone never refuses, excludes or holds a record in the fold. After shape, identity, repair-floor, pending-scope and capacity admission succeed, live validation judges a dependent record on its own reference: a match proceeds to library rules, a mismatch is refused TapeContradiction without appending an exclusion, and NotYet is refused Unresolved. Refusing write, verify and finalize on a contradicted tape is D3 admission policy for B4b-2/B5, not a fold rule. During replay and resolution each dependent record is judged on its own reference, after shape and tape identity checks and before coverage, chains or uniqueness; on receipt, the RepairBelowReference floor check also precedes the own-reference check. A below-floor repair is refused before that reference is consulted: it is not received, is not excluded, and changes no summary. A replay or resolution mismatch reached after those admission checks excludes the record without applying it or retaining any scope, updates the tape's minimum contradicting sequence, and increments its excluded diagnostic count. A matching reference proceeds to library rules when its scopes are free; NotYet and earlier pending scopes still hold records. Resolution checks a scope-held dependent record's reference just as receive does. Per-tape summaries retain the minimum contradiction and highest received dependent sequence; map membership alone does not mean contradicted. FenceRaised is the immediate-effect exception described in tag 8. applied_through never crosses pending. Live/replay equivalence under growing immutable tape views requires dependent references to be present and matching at each append; FenceRaised.at is evidence and may be NotYet at its append. Excluded counters and unresolved_fence_evidence are diagnostics excluded from the equivalence comparison: unresolved fence evidence may differ between live and replay without changing fence enforcement or tape authority.

LibraryHistory first uses retained state; otherwise the store binary-searches existing segment headers and scans one segment. DecisionRef binds sequence and UUID; only FenceId wire references may use sequence-only lookup in the header-bound log. Replay sees only lower sequences. FOLD-VECTORS.json freezes ReleaseWithoutFence with an explicitly empty injected history: a contiguous store log cannot lack an earlier sequence, and a diagnostic at that sequence is WrongCounterpart. I/O faults are HistoryUnavailable. Memory is O(resources), plus lifetime idempotency key/value payload at 80 bytes per keyed operation (SHA-256 of length-prefixed actor_fingerprint, u16 kind, and idempotency_key maps to operation id and request_fingerprint; allocator/map overhead is measured separately), plus one entry per crash supersede or unprotected acceptance (tags 6 and 7, retained for lifetime lookup and uniqueness), plus pending up to 4096 records and 64 MiB of canonical CBOR envelopes/bodies, plus one looked-up frame. Tags 13, 14 and 26 require an OperationRecorded chain ending in a terminal record to be pruned; writers that omit that chain retain those slots indefinitely. The pending byte limit counts the canonical encoding of the whole structural map `{0: envelope, 1: body}`. Pending bytes use exact-length boxed storage. Exceeding either total refuses with PendingLimitExceeded { scope, records, bytes }, preserving every prior pending record. Append to a blocked scope at capacity returns the same typed error.

FRAMES.tsv lists complete decodable frames, including frames beyond the authoritative prefix and the decoded refused frame of a rejected case; authoritative records have sequence at most MANIFEST.last_sequence. Frames whose header or encoding cannot be decoded are not listed. MANIFEST.log_uuid is the first segment's on-disk header value; in wrong-log-uuid it differs from the expected identity. Tape views are external replay inputs published in VIEWS.tsv and README.md.
## Record tags, causal keys and scopes

Every record except FenceRaised and VerificationVerdict with `at: None` also orders on the current label of each tape it names in its direct tape set. For this rule the direct tape set is the tape field of tags 1, 3, 5, 14 and 25, both holders for tag 2, the TapeAt tape of tags 6, 7, 13, 15 and 26, and the optional tape or cartridge of tags 10, 17, 20, 21 and 24; other tags have no direct tapes. Reprovision adds its new and predecessor Tape scopes and its explicit new Label, but does not add the predecessor current label. A DecisionRef adds only the referenced record's direct scopes, defined separately from the ordering column below. These are Tape for its direct tape set above (also the scope tape for FenceRaised, and the new and predecessor tapes for Reprovision); Operation for the operation of tags 6, 7, 13, 14, 15, 17, 23 and 26, the optional operation of tags 8 and 20, the adoption operation of tag 4, the parent operation of tag 10, and the session_id of tag 24. Additional direct scopes are: the explicit new Label of tags 2 and 4; Endpoint of a drive FenceRaised; Endpoint of a Load/Unload MotionIntent, plus Library for a bay endpoint, or Library of a Move MotionIntent; Library or standalone Endpoint of tag 12; Config of tag 16, and Config of a Rule AssignPool or Rule Reprovision that references a configuration; Endpoint of tag 17; Drive and optional Endpoint of tag 18; Drive of tags 19 and 20; CleaningRun and optional Drive of tag 21; Alarm of tag 22; and optional Endpoint of tag 24. Thus a reference to a fence inherits its scope Tape or drive Endpoint plus its optional Operation, independent of tag 8's deferral column. Inheritance is one level only: no current labels, no scopes the referenced record itself inherited, and no Config from a referenced Rule AssignPool or Rule Reprovision. A reference to FenceReleased inherits nothing. FenceReleased's own ordering instead adds only the target Tape or Endpoint of the fence it names, and only while that fence is open (a raised fence still awaiting its queued check counts as open), as described in tag 9. A release of a fence that is no longer open orders on nothing.

| Tag | Record | Scopes | Causal rule |
|---:|---|---|---|
| 1 | AssignPool | Tape, current Label; also Config when `source` is `Rule` with a configuration reference | `change.expected` = the tape's `assignment_generation`. The tape is provisioned and not retired. `Rule` requires `configuration` to name the latest retained `ConfigurationAccepted`; an older reference is `StaleConfiguration`, including an identical retry. History classifies a mismatching reference: only an older ConfigurationAccepted is StaleConfiguration; another kind or absent reference is WrongCounterpart. `pool`, if present, is non-empty. An Operator assignment without a configuration reference has no Config scope; if it supplies a reference, the general historical-reference rule adds that referenced record's direct scopes |
| 2 | Label | Tape (both holders), Label (old and new) | `change` against the tape's `label_generation`. Labels are unique among live tapes. If the label is held by live tape X, `prior_holder` must name X with X's current `label_generation`. The one record moves the label, clears X's label and bumps X's generation. If the label is free, `prior_holder` is null. `observed_bot_uuid`, if present, equals `tape.uuid`. **Recovery 4** |
| 3 | Retire | Tape, Label | `assignment_generation` equals the current one. The tape is live. Retire ends the incarnation for good and releases its label and pool. `observed_tape_sequence` is evidence, never a prerequisite. Later live-target records cannot name a retired `TapeRef` (`RetiredIdentity`); `Reprovision.predecessor` may name its retired predecessor and `FenceRaised.at` remains evidential |
| 4 | Reprovision | Tape (new and predecessor), Label, Operation (adoption); also Config when `pool_source` is `Rule` | A new UUID, never seen before. `predecessor`, if present, is retired. The label, if any, is free. `label_provenance` is null exactly when `label` is. `adoption` is present exactly for `Adopt`. Mandatory MediumSpec owns the sole fixed block size (record_size > 0, multiple of 512 for adoption). record_size, rerecord, lifecycle_ops and family kind are fixed for this incarnation; changing them requires a new incarnation. Former key 5 block_size is removed; parity_scheme is key 5, medium key 6, pool key 7, endpoint key 8 and adoption key 9. Sets assignment, label and media generations to 1 in one record; no separate `Label` or `AssignPool` for the initial state. Keys 10 pool_source, 11 operation and 12 replaced_bot are optional. PoolSource is Operator(Unit)=0 or Rule(DecisionRef)=1. Rule must name the latest retained ConfigurationAccepted, exactly as tag 1 (StaleConfiguration/WrongCounterpart); a Rule Reprovision, like a Rule AssignPool, orders after the configuration it references (Config scope). `pool_source` is present exactly when `pool` is; otherwise the record is InvalidSchema, in shape validation before a Rule reference is judged. The fold does not check the presence coupling of operation to Initialize. The tape's recorded pool source is set by this record and replaced by the `source` of each later AssignPool (a Rule AssignPool records its configuration reference). replaced_bot is the overwritten BOT UUID, evidence only; it is InvalidSchema unless mode is Initialize and predecessor is None. No decision reads it. |
| 5 | MediumSpecSet | Tape | `change` against the live tape's `media_revision`. Family kind (Lto/Disk) is fixed. Within Lto, generation may change only when the incarnation's derived ever_measured bit is false; otherwise MediumFamilyChanged. Provisioning (tag 4) initializes ever_measured from its own source, and every committed MediumSpecSet with source Measured sets it. The bit never clears within an incarnation, even if source is later revised to Label or Operator; it is derived from records, not stored on the wire. record_size, rerecord and lifecycle_ops must equal their current values, otherwise ImmutableMediumField { field }. Only usable_logical_bytes and source are generally revisable, with the stated LTO correction exception. Capacity may stay equal or decrease; an increase requires source Measured on this new revision (fresh measurement), otherwise CapacityIncreaseRequiresMeasurement. A retry is not a fresh revision. Fixed geometry and capabilities can change only through a new incarnation |
| 6 | SupersedeTail | Tape, Operation | **Pending dependency** on `at`. Only this tag permits sequence 0, naming the existing native log's empty prefix: record_digest is SHA-256 of the exact immutable header bytes. Missing or replaced native identity contradicts it; it never raises the repair floor above 0. Other dependent tags and evidential FenceRaised.at still refuse sequence 0 as InvalidSchema. operation, session_id and preview_digest are nullable (keys remain mandatory); absent branch facts must not be fabricated. At most one per (`at.tape`, `session_id`), including session_id null. B4b's tape fold checks the `SessionEnded.crash_ack_audit_sequence` that names it (kind, tape, session, count); B2 provides `LibraryFold::supersede(sequence)` for that lookup |
| 7 | AcceptUnprotectedFinalization | Tape, Operation | **Pending** on `at`. `w < t`. At most one per (incarnation, operation). Codec, fold and fixtures exist now. **No production constructor**; step 6 adds the emitter |
| 8 | FenceRaised | scope Tape only for ordering; none for DriveFault | ContainerQuarantine (cause 12) and Model (scope 2) decode but are refused before queuing or committing with ReservedNotYetUsed. `Write` requires tape scope; `DriveFault` requires drive scope (`FenceScopeMismatch`). An unqueued tape-scoped fence requires a live tape. A live or replay fence takes effect immediately. It defers its live-tape check only behind pending records on `Scope::Tape` of its scope tape; its queue entry contains only that scope. Label, operation and endpoint scopes neither block it nor are blocked by it while queued. A DriveFault fence has no tape-ordered check and never defers. A fence on a tape unknown while that tape has queued records is accepted; if its tape is unknown, of another incarnation or retired at resolution, the check is dropped and the failure is attached to the open fence as its full TapeRef and cause. This creates no tape contradiction; releasing the fence removes the report. The check runs in tape queue order; B4b writers check fence targets themselves. Contradiction status never decides a fence check. A fence raised on a contradicted tape with no pending predecessor validates liveness immediately. A queued check remains pending during replay even if a predecessor is later excluded. The next `resolve`, which runs only before a live append, retries it in tape queue order; a remaining NotYet predecessor still holds the check. The fence stays open throughout. The fence id is this record's sequence. No deduplication: every raise is its own fence. **`at` is evidence, not a dependency:** the fence applies at once, and an unresolvable `at` is reported, never waited for. Key 6 detail is Bounded<DiagnosticEntry,256>, strictly ascending unique UTF-8 byte keys (InvalidSchema). The reason key is Text <=4096, presentation only. Cause 14 is NativeLogBehindProjection; cause 15 is unallocated and has no variant. Cause-14 typed evidence keys are listed below. A two-target event uses two records, one per scope; scope is never widened. |
| 9 | FenceReleased | target Tape or Endpoint of the named fence while it is open; none otherwise | `fence` remains a u64 FenceId: the raise sequence within the header-bound log UUID. Only this reference form uses sequence-only LibraryHistory lookup; DecisionRef always matches the record UUID. An earlier raise no longer open is `FenceAlreadyReleased`, even after later fences; another tag is `WrongCounterpart`; absent history is `ReleaseWithoutFence`. Zero/forward references remain `ForwardReference`. The latest identical release retry remains a no-op. A release that differs from an earlier release of the same fence is `FenceAlreadyReleased`. While the named fence is open, the release orders on its target tape or endpoint, without inheriting the raise's operation. A release of a fence that is no longer open, including an identical retry of an earlier release, orders on nothing. A release cannot resolve a motion, a pending record or an ambiguity. Key 3 detail is Bounded<DiagnosticEntry,256>, strictly ascending unique byte keys. Nonempty detail is InvalidSchema except on a forced release of a cause-14 fence; its typed keys are below. Empty detail is the definite empty array 0x80 and the body map has four entries. forced=true is an operator release without the cause's release proof; forced=false is a release made under the cause's release rule, which is either a producer's proof or an operator's acknowledgement where the fence accepts one. |
| 10 | MotionIntent | Tape, Endpoint, Library, Operation | `command` is unique among open intents; a `command` already open is `ConflictingDuplicate`. **The LOAD attempt count lives only here** (item 6): `attempt` is 1 plus the number of earlier `Load` intents with the same `parent_operation` and endpoint since that operation's count was last cleared; with no parent it is 1. A `ReadinessObserved` of the operation whose `result` is not `Pending` clears the count; a terminal `OperationRecorded` of the operation clears it only when no `Pending` readiness head of that id is retained. An intent with no outcome makes its library (a `Bay` endpoint's serial, a `Move`'s serial, or a standalone endpoint) **InventoryUnknown** |
| 11 | MotionOutcome | intent scopes | `intent` names an open `MotionIntent` (`OutcomeWithoutOpenIntent`; `WrongCounterpart` if another tag). An intent reference naming an existing sequence with a different record UUID is `OutcomeWithoutOpenIntent`. One outcome per intent. `Done` requires a confirmation: Inventory for Move and Unload; Readiness for Load when a readiness flow follows, otherwise Inventory (including cleaning). The writer validates that the sequence exists and has the confirmation kind. An inventory confirmation must observe without resolving that intent; the following Done consumes it. `Unknown` leaves the library InventoryUnknown |
| 12 | InventoryReconciled | observed Library or standalone Endpoint; resolved intent scopes | Key 0 is InventorySubject: [0, library serial] or [1, Standalone map]. Observation generation strictly increases per subject. Library observations use element status; a partial observation resolves only intents whose affected elements are all listed. Reconciliation clears exactly the named uncertainty and creates no placement ownership. Standalone observations use TEST UNIT READY plus READ POSITION, require complete=true and empty elements, and resolve only open or Unknown LOAD/UNLOAD intents on that exact endpoint. Naming another endpoint or a changer intent is InventoryScope; stale generation is InventoryGeneration. Each resolved intent is consumed once. Confirmation observations leave resolves empty, followed by MotionOutcome Done; never resolve and then append an outcome for the same intent |
| 13 | VerificationProgress | Tape, Operation | **Pending** on `at`. Within one operation, `at`, `plan_digest` and `modes` never change (`CoverageChanged`). `parts_done` strictly increases and `next` strictly advances in lexicographic order (`ProgressRegressed`). `next.part = parts_done` (`ImpossibleCursor`). Progress is never a verdict. **No production constructor in step 4** (item 7); step 7 emits it. modes uses the nonzero seven-bit verification mask below; zero or any bit 7–31 is InvalidSchema. |
| 14 | VerificationVerdict | Tape (`tape`), Operation; progress scopes when referenced | at (key 1) is optional; tape (key 7) and outcome (key 8) are mandatory. Some(at) requires at.tape == tape (InvalidSchema) and waits on that native proof. None is a producer assertion that no native reference existed at recording time; the fold does not check it. Receive and replay accept None without native lookup, preserve normal pending Tape/Operation order only (no label scopes), record no floor and invent no native sequence. Outcomes are VerifiedComplete=0, VerifiedDegraded=1, RecoveryRequired=2. VerifiedComplete requires damaged empty; the other outcomes may have no ranges. Each range requires start < end; unprotected retains its independent unprotected-tail meaning, w < t. modes uses the nonzero seven-bit table below (InvalidSchema for zero or bits 7–31). progress still names the latest progress for the operation with identical at and modes (VerdictWithoutCoverage). The producer records the verdict before the operation's `Finished`; the fold does not check this order. The verdict supersedes only that operation's progress. A referenced verdict at N says nothing about N+1; None says nothing about any native sequence |
| 15 | ManualFinalizationAccepted | Tape, Operation | **Pending** on `at`, which names the tape log's `FinalizationPrepared` record; its digest binds the immutable plan, so there is no separate plan digest. `assignment_generation` equals the tape's current one (`GenerationMismatch`; item 9's "Accepted with a stale generation"). One per incarnation. Retired in step 6 |
| 16 | ConfigurationAccepted | Config | `generation` is the previous one plus 1, starting at 1. `config_digest` = SHA-256 of the canonical bytes of `definitions` (`ConfigDigest`). It holds only catalog-relevant fields, never a secret or a TLS key. Assignments still need `AssignPool`; replay never reinterprets an older config |
| 17 | ReadinessObserved | Tape, Endpoint, Operation, previous and intent scopes | `scope` (key 13) is Endpoint=0 by default or Library=1; Library requires a bay endpoint and projects selected-library-snapshot admission uncertainty across all bays. `media` (key 14) is an optional MediumFamily, allowed only without `tape`; with a tape the family is its MediumSpec and supplying media is InvalidSchema. `previous` names the last `ReadinessObserved` of the same operation, and is null for its first. `load_intent`, if present, names an open or resolved `Load` intent on the same endpoint; resolved records use LibraryHistory. **It carries no LOAD count** (item 6). `ready_generation` strictly increases per endpoint. A terminal `result` closes the operation. An operation's end does not end its readiness chain; only a readiness `result` other than `Pending` does. In fold terms, a terminal `OperationRecorded` of the same id leaves a retained `Pending` head and that operation's LOAD count in place; both end at the first `result` other than `Pending`. Terminal is enforced for chained successors; reuse of a closed unkeyed operation id is not detected in fold state. B4b writers mint fresh ids. limit_ms (key 8) is Option<u64>: None means no deadline, with no sentinel. Keys 15 source_slot (Option<u16>), 16 run (Option<Id>), and 17 cancel_source (Option<CancelSource>) are optional. CancelSource codes are Operator=0, Shutdown=1, StartupReconcile=2. Phases 49 LoadDriveReadiness and 50 LoadDriveCompletion are distinct, with no aliases. Device paths are not recorded; presentation belongs in Diagnostic. |
| 18 | DriveObserved | Drive, Endpoint | The first observation creates the drive with `stewardship_generation = 1`. The observation time is the envelope's |
| 19 | DriveDecision | Drive | The drive is known. `change` against `stewardship_generation`. Retire requires active; Reinstate requires retired. Fencing is tags 8 and 9, never this |
| 20 | DriveHealthObserved | Drive, Tape, Operation | The drive is known. A non-null (`session_id`, `trigger`) matching the drive's latest retained snapshot is a no-op if the body is identical, otherwise `ConflictingDuplicate`. The fold makes no lifetime uniqueness claim. B4's health projector must retain the (`session_id`, `trigger`) unique key with first-wins: ignore an identical later body; report a different later body as a projection conflict and never apply it. Null-session snapshots are independent. Counters are cumulative per mount |
| 21 | CleaningRecorded | CleaningRun, Drive, Tape | `Begin` carries only trigger, with no home slot. `Advance` key 1 is home_slot (Option<u16>), present exactly when phase is MovingIn; that record must also name the selected cartridge. Missing slot/cartridge at MovingIn or a slot at any other phase is InvalidSchema. MovingBack returns to the run's MovingIn slot, retained through omitted cartridge/slot fields and delayed drive binding. Register.home_slot remains the initial home, and the projected cartridge home follows the latest run. `previous` chains a run's records. At most one active run per drive and per cartridge (today's unique indexes). Register, Expire and Reject concern the cartridge, not a run. Reject(Reason), wire code 5, requires a registered cartridge that is neither expired nor rejected (UnknownTape, WrongMediumKind, or RetiredIdentity otherwise). It permanently prevents selection and verified credit, including credit through an inherited run binding. It projects cleaning_state=rejected, distinct from expired. Reject precedes the separate Finish(Failed): a crash between them leaves an ineligible cartridge and an active run; replay cannot make it eligible. Failed closure and return motion remain legal after rejection. Reject has no previous run reference and does not advance the run chain. Register creates an inventory-only cleaning entity without MediumSpec or BOT identity: cartridge must be UUIDv5(NAMESPACE_OID, UTF-8 `remanence:cleaning-cartridge:{barcode}`), incarnation 1, previously unknown as either medium kind, except that ruling 17 permits a retired data UUID after Retire releases its label. The data retirement remains historical; a live data UUID still conflicts. The barcode must be nonempty and trimmed. A known live data or any cleaning UUID is ConflictingDuplicate; a malformed identity is InvalidSchema. Only Register exempts its cartridge from UnknownTape. Expire ends the cleaning entity permanently; it cannot be registered again. An inherited run cartridge cannot bypass expiry to advance or receive verified credit; Finish without verified credit may close the run. Data-tape records naming a cleaning cartridge, including Reprovision.predecessor and every dependent record, fail WrongMediumKind before native lookup. WrongMediumKind is fold-only, with no new wire code. The projector creates a cleaning tape row with null block_size and the home slot. **The fold never derives a fence from cleaning** (item 11): a run that needs an operator is a separate `FenceRaised(DriveFault)` |
| 22 | AlarmChanged | Alarm, cause scopes | `change` against the alarm generation (0 when never raised). Raise requires absent or cleared; Acknowledge requires raised and unacknowledged, with an envelope actor other than System (the acknowledger is the actor, recorded once); Clear requires raised. Refresh (action code 3, payload keys severity=0/reason=1/cause=2) requires raised, acknowledged or not, advances generation by CAS, replaces severity/reason/cause, and preserves acknowledgement and its actor; a subsequent acknowledgement still refuses. `cause` may name any earlier record, using LibraryHistory after replacement. AlarmKind adds DriveSerialCollision=11 and RetiredDriveReappeared=12. |
| 23 | OperationRecorded | Operation | `previous` chains the operation's records. Accepted comes first or not at all. Started follows nothing or Accepted. Cancel records follow a non-terminal state. Finished, Failed, CancelledBeforeDispatch, CompletedAfterCancel and CompletionUnknown are terminal; nothing follows them. The idempotency scope (`actor_fingerprint`, `kind`, `idempotency_key`) is unique: a second Accepted for that scope with a different operation or fingerprint is a `ConflictingDuplicate`. An identical keyed retry yields `Duplicate(original receipt)` while Accepted is latest, `Chain` after Started or another non-terminal successor, or `ClosedOperationReplay { operation }` after the operation closes. B4b's API must answer all three from its projection, treating `Chain` for an identical keyed Accepted as already running and using the operation id for a closed retry because the fold does not retain terminal receipts. Terminal is enforced for chained successors; reuse of a closed unkeyed operation id is not detected in fold state. B4b writers mint fresh ids. **No progress variant**: progress gates nothing in step 4 and stays `Diagnostic`. Failed.refusal_code names the RefusalCode table under Failed. |
| 24 | SessionObserved | Operation(session), Tape, Endpoint | Opened comes first; the other events are terminal. **A write session cannot be expressed**: write sessions are the tape log's (item 5b), and kind 0 is refused. It has no receipts, because durable receipts are tape-log only |
| 25 | WrapMapCached | Tape | `digest` = SHA-256(`descriptors`), whose bytes are today's descriptor serialisation, opaque to the fold. Only digest and calibration_generation are retained per tape; the projector reads descriptor bytes from the source record. `calibration_generation` never decreases per tape. The RAO control store (the third authority) still decides servability; this rebuilds only the SQL cache |
| 26 | RecoveryRecorded | Tape, Operation, prior scopes | **Pending** on `at`. RepairApplied requires `retained_through = at.sequence`; a mismatch is InvalidSchema in shape validation, before any view lookup. On receipt or live validation, RepairBelowReference { highest } refuses a repair below any earlier received dependent reference on that tape, whether applied, duplicate, pending or excluded. Duplicates count toward the floor; refused candidates do not. A prior repair is itself a reference, so repairs are monotone. LibraryFold::highest_reference(tape) exposes the same floor for the future repair writer. An admitted repair is an ordinary dependent record, judged on its own reference; it neither clears contradictions nor undoes records. Resolution does not reapply the floor against references received after that repair. **`AppendRecoveryRequired` does not exist** (item 5c): action 0 is refused. Missing native proof stays pending. `prior` must name an earlier RecoveryRecorded, including one replaced in retained state |
| 32 | Diagnostic | none | Detail keys are strictly ascending and unique (InvalidSchema); a nested value is refused at encode. **The fold validates and ignores it.** `DiagnosticEvent` is B2's own closed enum (below). DiagnosticEvent 20 RefusalRecorded requires code (Unsigned, an allocated RefusalCode from the Failed table) and name (Text); subject_kind/subject_id carry its subject. |

All generation failures use GenerationMismatch. RepairBelowReference { highest } is a fold-only refusal with no new wire code. Shape violations use InvalidSchema; pair/type failures use WrongCounterpart or the named rule error. For a dependent record, digest mismatch is a scoped `TapeContradiction`, checked after shape and tape identity and before other library rules in validate_at. The receipt-time repair floor is also enforced for a scope-held repair, independent of its native view. A matching digest with the wrong native kind is the typed refusal `CounterpartKind`: replay stops there, and nothing is held for that record. Only tag 15 constrains the kind (`FinalizationPrepared`). Tag 3 adds mandatory key 4, detail: text <=4096 bytes or null; it is presentation only. Tag 21 is removed from production orchestration in step 5; its historical decoder remains.

### Additional wire conventions

Id is b16; Hash is b32; Text/TextValue is UTF-8 text <=4096 bytes; LabelText is UTF-8 text <=32 bytes. Blob<N> is a CBOR byte string <=N bytes; Bounded<T,N> is a definite array <=N entries. Integer types are unsigned, except Negative, the diagnostic i64 <0. Struct names are mandatory ascending integer-keyed maps. Unit is null. DiagnosticEntry is [text,DiagnosticValue]. AuditActor is {0: kind (System=0, User=1, Service=2), 1: name (null exactly for System)}. Pool watermark fields encode the existing f64 IEEE-754 bits as u64, preserving exact configuration values without CBOR floats; they are not text.

### Verification modes (tags 13 and 14)

modes is u32, nonzero; bits 7–31 must be zero (InvalidSchema).

| Bit | Name | Meaning |
|---:|---|---|
| 0 | TERMINAL_INDEX | Five terminal components validated |
| 1 | PHYSICAL_PREFIX | Every physical prefix row read; verified_prefix_record_count present |
| 2 | PROTECTED_CONTENT | REM-PARITY 2.2 protected-content walk ran |
| 3 | REPLICAS | replica_health evaluated |
| 4 | SEPARATIONS | separation_health evaluated |
| 5 | EOD_MEASURED | measured_eod_lba obtained from the drive |
| 6 | BOT_RECOVERY | verification_basis=bot_structural_recovery; recovery_inventory attached |

### Fence detail keys

All detail arrays reuse DiagnosticEntry and are limited to 256 entries. Keys are unique and strictly ascending in UTF-8 byte order; producers construct them through BTreeMap<Text, DiagnosticValue>. Human-readable reason is presentation only and never selects a decision.

FenceRaised cause 14 uses the following typed evidence keys (in byte order):

| Key | DiagnosticValue type / meaning |
|---|---|
| basis | Text: floor, mark, both |
| conflict_digests | Bytes: 32 bytes per digest |
| floor_digest | Bytes |
| floor_named_by | Unsigned |
| floor_sequence | Unsigned; may be zero with floor_digest |
| mark_offset | Unsigned |
| mark_sequence | Unsigned |
| observed_closed_end | Unsigned |
| observed_floor | Text: absent, behind, digest_conflict, digest_mismatch |
| observed_last_closed | Unsigned |
| observed_mark | Text: absent, behind, short_bytes, digest_mismatch |
| source_byte_end | Unsigned |
| source_digest | Bytes |

A zero floor sequence is evidence in detail; FenceRaised.at.sequence must remain nonzero. The optional reason key is Text <=4096 bytes. The fold validates detail ordering; the typed evidence vocabulary describes the producer contract.

FenceReleased detail is empty except for a forced release of a cause-14 fence. That release uses these seven keys in byte order:

| Key | DiagnosticValue type / meaning |
|---|---|
| basis | Text: floor |
| floor_digest | Bytes |
| floor_sequence | Unsigned |
| observed_closed_end | Unsigned |
| observed_floor | Text |
| observed_last_closed | Unsigned |
| observed_prefix_digest | Bytes |

## Worked Diagnostic at sequence 1

Record hash `0d4fc96d6c9a0162617edea6cb4ec7a25dfcbcfdf53cb594e03ba6316768bb02`.

```text
01002000630000002000000001000000000000003eda05ea491d6d79a200aa0050c0000000000000000000000000000001011b18e45fe7da02ca00026c666978747572652d686f737403191092047172656d616e656e63652d6669787475726505a2000001f6060307f608f609f601a4000001676669787475726502f6038099a9901a3cbf7bb90d4fc96d6c9a0162617edea6cb4ec7a25dfcbcfdf53cb594e03ba6316768bb027c231966e661e2e3
```

| Offset | Hex byte | Section |
|---:|---|---|
| 0 | 01 | version |
| 1 | 00 | version |
| 2 | 20 | tag |
| 3 | 00 | tag |
| 4 | 63 | structural length |
| 5 | 00 | structural length |
| 6 | 00 | structural length |
| 7 | 00 | structural length |
| 8 | 20 | payload length |
| 9 | 00 | payload length |
| 10 | 00 | payload length |
| 11 | 00 | payload length |
| 12 | 01 | sequence |
| 13 | 00 | sequence |
| 14 | 00 | sequence |
| 15 | 00 | sequence |
| 16 | 00 | sequence |
| 17 | 00 | sequence |
| 18 | 00 | sequence |
| 19 | 00 | sequence |
| 20 | 3e | prefix CRC |
| 21 | da | prefix CRC |
| 22 | 05 | prefix CRC |
| 23 | ea | prefix CRC |
| 24 | 49 | prefix CRC |
| 25 | 1d | prefix CRC |
| 26 | 6d | prefix CRC |
| 27 | 79 | prefix CRC |
| 28 | a2 | canonical structural CBOR |
| 29 | 00 | canonical structural CBOR |
| 30 | aa | canonical structural CBOR |
| 31 | 00 | canonical structural CBOR |
| 32 | 50 | canonical structural CBOR |
| 33 | c0 | canonical structural CBOR |
| 34 | 00 | canonical structural CBOR |
| 35 | 00 | canonical structural CBOR |
| 36 | 00 | canonical structural CBOR |
| 37 | 00 | canonical structural CBOR |
| 38 | 00 | canonical structural CBOR |
| 39 | 00 | canonical structural CBOR |
| 40 | 00 | canonical structural CBOR |
| 41 | 00 | canonical structural CBOR |
| 42 | 00 | canonical structural CBOR |
| 43 | 00 | canonical structural CBOR |
| 44 | 00 | canonical structural CBOR |
| 45 | 00 | canonical structural CBOR |
| 46 | 00 | canonical structural CBOR |
| 47 | 00 | canonical structural CBOR |
| 48 | 01 | canonical structural CBOR |
| 49 | 01 | canonical structural CBOR |
| 50 | 1b | canonical structural CBOR |
| 51 | 18 | canonical structural CBOR |
| 52 | e4 | canonical structural CBOR |
| 53 | 5f | canonical structural CBOR |
| 54 | e7 | canonical structural CBOR |
| 55 | da | canonical structural CBOR |
| 56 | 02 | canonical structural CBOR |
| 57 | ca | canonical structural CBOR |
| 58 | 00 | canonical structural CBOR |
| 59 | 02 | canonical structural CBOR |
| 60 | 6c | canonical structural CBOR |
| 61 | 66 | canonical structural CBOR |
| 62 | 69 | canonical structural CBOR |
| 63 | 78 | canonical structural CBOR |
| 64 | 74 | canonical structural CBOR |
| 65 | 75 | canonical structural CBOR |
| 66 | 72 | canonical structural CBOR |
| 67 | 65 | canonical structural CBOR |
| 68 | 2d | canonical structural CBOR |
| 69 | 68 | canonical structural CBOR |
| 70 | 6f | canonical structural CBOR |
| 71 | 73 | canonical structural CBOR |
| 72 | 74 | canonical structural CBOR |
| 73 | 03 | canonical structural CBOR |
| 74 | 19 | canonical structural CBOR |
| 75 | 10 | canonical structural CBOR |
| 76 | 92 | canonical structural CBOR |
| 77 | 04 | canonical structural CBOR |
| 78 | 71 | canonical structural CBOR |
| 79 | 72 | canonical structural CBOR |
| 80 | 65 | canonical structural CBOR |
| 81 | 6d | canonical structural CBOR |
| 82 | 61 | canonical structural CBOR |
| 83 | 6e | canonical structural CBOR |
| 84 | 65 | canonical structural CBOR |
| 85 | 6e | canonical structural CBOR |
| 86 | 63 | canonical structural CBOR |
| 87 | 65 | canonical structural CBOR |
| 88 | 2d | canonical structural CBOR |
| 89 | 66 | canonical structural CBOR |
| 90 | 69 | canonical structural CBOR |
| 91 | 78 | canonical structural CBOR |
| 92 | 74 | canonical structural CBOR |
| 93 | 75 | canonical structural CBOR |
| 94 | 72 | canonical structural CBOR |
| 95 | 65 | canonical structural CBOR |
| 96 | 05 | canonical structural CBOR |
| 97 | a2 | canonical structural CBOR |
| 98 | 00 | canonical structural CBOR |
| 99 | 00 | canonical structural CBOR |
| 100 | 01 | canonical structural CBOR |
| 101 | f6 | canonical structural CBOR |
| 102 | 06 | canonical structural CBOR |
| 103 | 03 | canonical structural CBOR |
| 104 | 07 | canonical structural CBOR |
| 105 | f6 | canonical structural CBOR |
| 106 | 08 | canonical structural CBOR |
| 107 | f6 | canonical structural CBOR |
| 108 | 09 | canonical structural CBOR |
| 109 | f6 | canonical structural CBOR |
| 110 | 01 | canonical structural CBOR |
| 111 | a4 | canonical structural CBOR |
| 112 | 00 | canonical structural CBOR |
| 113 | 00 | canonical structural CBOR |
| 114 | 01 | canonical structural CBOR |
| 115 | 67 | canonical structural CBOR |
| 116 | 66 | canonical structural CBOR |
| 117 | 69 | canonical structural CBOR |
| 118 | 78 | canonical structural CBOR |
| 119 | 74 | canonical structural CBOR |
| 120 | 75 | canonical structural CBOR |
| 121 | 72 | canonical structural CBOR |
| 122 | 65 | canonical structural CBOR |
| 123 | 02 | canonical structural CBOR |
| 124 | f6 | canonical structural CBOR |
| 125 | 03 | canonical structural CBOR |
| 126 | 80 | canonical structural CBOR |
| 127 | 99 | structural CRC |
| 128 | a9 | structural CRC |
| 129 | 90 | structural CRC |
| 130 | 1a | structural CRC |
| 131 | 3c | structural CRC |
| 132 | bf | structural CRC |
| 133 | 7b | structural CRC |
| 134 | b9 | structural CRC |
| 135 | 0d | record hash |
| 136 | 4f | record hash |
| 137 | c9 | record hash |
| 138 | 6d | record hash |
| 139 | 6c | record hash |
| 140 | 9a | record hash |
| 141 | 01 | record hash |
| 142 | 62 | record hash |
| 143 | 61 | record hash |
| 144 | 7e | record hash |
| 145 | de | record hash |
| 146 | a6 | record hash |
| 147 | cb | record hash |
| 148 | 4e | record hash |
| 149 | c7 | record hash |
| 150 | a2 | record hash |
| 151 | 5d | record hash |
| 152 | fc | record hash |
| 153 | bc | record hash |
| 154 | fd | record hash |
| 155 | f5 | record hash |
| 156 | 3c | record hash |
| 157 | b5 | record hash |
| 158 | 94 | record hash |
| 159 | e0 | record hash |
| 160 | 3b | record hash |
| 161 | a6 | record hash |
| 162 | 31 | record hash |
| 163 | 67 | record hash |
| 164 | 68 | record hash |
| 165 | bb | record hash |
| 166 | 02 | record hash |
| 167 | 7c | payload CRC |
| 168 | 23 | payload CRC |
| 169 | 19 | payload CRC |
| 170 | 66 | payload CRC |
| 171 | e6 | payload CRC |
| 172 | 61 | payload CRC |
| 173 | e2 | payload CRC |
| 174 | e3 | payload CRC |
