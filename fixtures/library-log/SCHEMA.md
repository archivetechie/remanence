# Library log v1 schema

Header: magic REMLIB\0\x01 (0..8), version u16 LE (8..10), log UUID b16 (10..26), first sequence u64 LE (26..34), previous hash b32 (34..66), valid ASCII date (66..76), CRC-64/XZ (76..84).

Frame: prefix version:u16/tag:u16/structural_len:u32/payload_len:u32/sequence:u64, all LE; prefix CRC (20..28); structural CBOR; structural CRC; 32-byte record hash; payload CRC. Overhead 44 bytes, combined cap 64 MiB.

record_hash = SHA-256(previous_hash || prefix20 || structural). First previous hash is zero. No CRC or hash covers itself. Structural map is {0: envelope, 1: body}. All keys and fields below are mandatory, with null for absent Option. TextValue/Text max 4096 UTF-8 bytes; LabelText max 32; Blob<N> and Bounded<T,N> enforce N before allocation. Pure enums are unsigned; unions are [code,payload], including null unit payloads.

| Header offset | Bytes | Field |
|---:|---:|---|
| 0 | 8 | REMLIB magic |
| 8 | 2 | Version 1 |
| 10 | 16 | Log UUID |
| 26 | 8 | First sequence |
| 34 | 32 | Previous segment hash |
| 66 | 10 | ASCII date |
| 76 | 8 | CRC of bytes 0..76 |

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

## Media

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | generation | `LtoGen` | false |
| 1 | source | `GenerationSource` | false |

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

## Finish

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | result | `CleanResult` | false |
| 1 | verified_credit | `bool` | false |

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
| 5 | block_size | `u32` | false |
| 6 | parity_scheme | `Option<ParityScheme>` | true |
| 7 | media | `Option<Media>` | true |
| 8 | pool | `Option<Text>` | true |
| 9 | endpoint | `Option<DriveKey>` | true |
| 10 | adoption | `Option<Adoption>` | true |

## MediaGenerationSet

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | tape | `TapeRef` | false |
| 1 | generation | `LtoGen` | false |
| 2 | source | `GenerationSource` | false |
| 3 | change | `GenerationChange` | false |

## SupersedeTail

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | at | `TapeAt` | false |
| 1 | operation | `Id` | false |
| 2 | session_id | `Id` | false |
| 3 | preview_digest | `Hash` | false |
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

## FenceReleased

| Key | Field | CBOR type / bound | Optional |
|---:|---|---|---|
| 0 | fence | `u64` | false |
| 1 | evidence | `Text` | false |
| 2 | forced | `bool` | false |

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
| 0 | library_serial | `Text` | false |
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
| 1 | at | `TapeAt` | false |
| 2 | damaged | `Bounded<DamagedRange, 4096>` | false |
| 3 | unprotected | `Option<Unprotected>` | true |
| 4 | modes | `u32` | false |
| 5 | edition_digest | `Option<Hash>` | true |
| 6 | progress | `Option<DecisionRef>` | true |

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
| 8 | limit_ms | `u64` | false |
| 9 | evidence | `Option<CompletionEvidence>` | true |
| 10 | load_intent | `Option<DecisionRef>` | true |
| 11 | ready_generation | `u64` | false |
| 12 | result | `ReadinessResult` | false |

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

### DriveKey

| Code | Variant / payload |
|---:|---|
| 0 | `Bay(Bay)` |
| 1 | `Standalone(Standalone)` |

### FenceScope

| Code | Variant / payload |
|---:|---|
| 0 | `Tape(TapeRef)` |
| 1 | `Drive(DriveKey)` |

### Motion

| Code | Variant / payload |
|---:|---|
| 0 | `Load(Load)` |
| 1 | `Unload(Unload)` |
| 2 | `Move(Move)` |

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

### AlarmAction

| Code | Variant / payload |
|---:|---|
| 0 | `Raise(AlarmRaise)` |
| 1 | `Acknowledge(Unit)` |
| 2 | `Clear(Reason)` |

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

## Replay and pending

Tags 6, 7, 13, 14, 15 and 26 depend on TapeAt; tag 15 requires native FinalizationPrepared. Present has both kind and digest. NotYet queues all touched scopes in source order, except FenceRaised always applies. resolve retries to a fixed point. Byte faults and library FoldError stop at the previous authoritative prefix. TapeContradiction isolates its tape and continues. applied_through never crosses pending.

LibraryHistory first uses retained state; otherwise the store binary-searches existing segment headers and scans one segment. DecisionRef binds sequence and UUID; only FenceId wire references may use sequence-only lookup in the header-bound log. Replay sees only lower sequences. FOLD-VECTORS.json freezes ReleaseWithoutFence with an explicitly empty injected history: a contiguous store log cannot lack an earlier sequence, and a diagnostic at that sequence is WrongCounterpart. I/O faults are HistoryUnavailable. Memory is O(resources), plus lifetime idempotency key/value payload at 80 bytes per keyed operation (SHA-256 of length-prefixed actor_fingerprint, u16 kind, and idempotency_key maps to operation id and request_fingerprint; allocator/map overhead is measured separately), plus one entry per crash supersede or unprotected acceptance (tags 6 and 7, retained for lifetime lookup and uniqueness), plus pending up to 4096 records and 64 MiB of canonical CBOR envelopes/bodies, plus one looked-up frame. Tags 13, 14 and 26 require an OperationRecorded chain ending in a terminal record to be pruned; writers that omit that chain retain those slots indefinitely. Pending bytes use exact-length boxed storage. Exceeding either total refuses with PendingLimitExceeded { scope, records, bytes }, preserving every prior pending record. Append to a blocked scope at capacity returns the same typed error.
## Record tags, causal keys and scopes

| Tag | Record | Scopes | Causal rule |
|---:|---|---|---|
| 1 | AssignPool | Tape, Config | `change.expected` = the tape's `assignment_generation`. The tape is provisioned and not retired. `Rule` requires `configuration` to name the latest retained `ConfigurationAccepted`; an older reference is `StaleConfiguration`, including an identical retry. History classifies a mismatching reference: only an older ConfigurationAccepted is StaleConfiguration; another kind or absent reference is WrongCounterpart. `pool`, if present, is non-empty |
| 2 | Label | Tape (both holders), Label (old and new) | `change` against the tape's `label_generation`. Labels are unique among live tapes. If the label is held by live tape X, `prior_holder` must name X with X's current `label_generation`. The one record moves the label, clears X's label and bumps X's generation. If the label is free, `prior_holder` is null. `observed_bot_uuid`, if present, equals `tape.uuid`. **Recovery 4** |
| 3 | Retire | Tape, Label | `assignment_generation` equals the current one. The tape is live. Retire ends the incarnation for good and releases its label and pool. `observed_tape_sequence` is evidence, never a prerequisite. No later record except `Diagnostic` may name a retired `TapeRef` (`RetiredIdentity`) |
| 4 | Reprovision | Tape (new and predecessor), Label | A new UUID, never seen before. `predecessor`, if present, is retired. The label, if any, is free. `label_provenance` is null exactly when `label` is. `adoption` is present exactly for `Adopt`. Sets assignment, label and media generations to 1 in one record; no separate `Label` or `AssignPool` for the initial state |
| 5 | MediaGenerationSet | Tape | `change` against the tape's `media_revision`. The tape is live |
| 6 | SupersedeTail | Tape, Operation | **Pending dependency** on `at`. At most one per (`at.tape`, `session_id`). B4b's tape fold checks the `SessionEnded.crash_ack_audit_sequence` that names it (kind, tape, session, count); B2 provides `LibraryFold::supersede(sequence)` for that lookup |
| 7 | AcceptUnprotectedFinalization | Tape, Operation | **Pending** on `at`. `w < t`. At most one per (incarnation, operation). Codec, fold and fixtures exist now. **No production constructor**; step 6 adds the emitter |
| 8 | FenceRaised | Tape or Endpoint, Operation | `Write` requires tape scope; `DriveFault` requires drive scope (`FenceScopeMismatch`). The tape is live. A live or replay fence behind queued state takes effect immediately; its live-tape check covers only the scope tape, runs in queue order and failure becomes a scoped contradiction. A live fence on a tape unknown while its operation has queued records is accepted and becomes a scoped contradiction on resolution, as on replay; B4b writers check fence targets themselves. If the scope tape is already contradicted or becomes contradicted while queued, the check resolves immediately, releasing its queued entry and other scopes while the contradiction stands and the fence stays open. The fence id is this record's sequence. No deduplication: every raise is its own fence. **`at` is evidence, not a dependency:** the fence applies at once, and an unresolvable `at` is reported, never waited for |
| 9 | FenceReleased | raised fence scopes | `fence` remains a u64 FenceId: the raise sequence within the header-bound log UUID. Only this reference form uses sequence-only LibraryHistory lookup; DecisionRef always matches the record UUID. An earlier raise no longer open is `FenceAlreadyReleased`, even after later fences; another tag is `WrongCounterpart`; absent history is `ReleaseWithoutFence`. Zero/forward references remain `ForwardReference`. The latest identical release retry remains a no-op. `forced` changes nothing in the fold but the fence: it cannot resolve a motion, a pending record or an ambiguity |
| 10 | MotionIntent | Tape, Endpoint, Library, Operation | `command` is unique among open intents. **The LOAD attempt count lives only here** (item 6): `attempt` is 1 plus the number of earlier `Load` intents with the same `parent_operation` and endpoint; with no parent it is 1. An intent with no outcome makes its library (a `Bay` endpoint's serial, a `Move`'s serial, or a standalone endpoint) **InventoryUnknown** |
| 11 | MotionOutcome | intent scopes | `intent` names an open `MotionIntent` (`OutcomeWithoutOpenIntent`; `WrongCounterpart` if another tag). One outcome per intent. `Done` requires a confirmation: Inventory for Move and Unload, Readiness for Load. `Unknown` leaves the library InventoryUnknown |
| 12 | InventoryReconciled | Library, resolved intent scopes | `observation_generation` strictly increases per library. `resolves` names intents of this library whose outcome is `Unknown` or absent. A partial poll (`complete = false`) may resolve only intents whose elements it lists. It clears exactly the named uncertainty and creates no placement ownership |
| 13 | VerificationProgress | Tape, Operation | **Pending** on `at`. Within one operation, `at`, `plan_digest` and `modes` never change (`CoverageChanged`). `parts_done` strictly increases and `next` strictly advances in lexicographic order (`ProgressRegressed`). `next.part = parts_done` (`ImpossibleCursor`). Progress is never a verdict. **No production constructor in step 4** (item 7); step 7 emits it |
| 14 | VerificationVerdict | Tape, Operation, progress scopes | **Pending** on `at`. Healthy means `damaged` is empty and `unprotected` is null; findings may combine. Each range has `start < end`; `w < t`. If the operation has progress records, `progress` must name the last one, with the same `at` (`VerdictWithoutCoverage`). It supersedes that operation's progress only. A verdict at tape sequence N says nothing about N+1 |
| 15 | ManualFinalizationAccepted | Tape, Operation | **Pending** on `at`, which names the tape log's `FinalizationPrepared` record; its digest binds the immutable plan, so there is no separate plan digest. `assignment_generation` equals the tape's current one (`GenerationMismatch`; item 9's "Accepted with a stale generation"). One per incarnation. Retired in step 6 |
| 16 | ConfigurationAccepted | Config | `generation` is the previous one plus 1, starting at 1. `config_digest` = SHA-256 of the canonical bytes of `definitions` (`ConfigDigest`). It holds only catalog-relevant fields, never a secret or a TLS key. Assignments still need `AssignPool`; replay never reinterprets an older config |
| 17 | ReadinessObserved | Tape, Endpoint, Operation, previous and intent scopes | `previous` names the last `ReadinessObserved` of the same operation, and is null for its first. `load_intent`, if present, names an open or resolved `Load` intent on the same endpoint; resolved records use LibraryHistory. **It carries no LOAD count** (item 6). `ready_generation` strictly increases per endpoint. A terminal `result` closes the operation. Terminal is enforced for chained successors; reuse of a closed unkeyed operation id is not detected in fold state. B4b writers mint fresh ids |
| 18 | DriveObserved | Drive, Endpoint | The first observation creates the drive with `stewardship_generation = 1`. The observation time is the envelope's |
| 19 | DriveDecision | Drive | The drive is known. `change` against `stewardship_generation`. Retire requires active; Reinstate requires retired. Fencing is tags 8 and 9, never this |
| 20 | DriveHealthObserved | Drive, Tape, Operation | The drive is known. A non-null (`session_id`, `trigger`) matching the drive's latest retained snapshot is a no-op if the body is identical, otherwise `ConflictingDuplicate`. The fold makes no lifetime uniqueness claim. B4's health projector must retain the (`session_id`, `trigger`) unique key with first-wins: ignore an identical later body; report a different later body as a projection conflict and never apply it. Null-session snapshots are independent. Counters are cumulative per mount |
| 21 | CleaningRecorded | CleaningRun, Drive, Tape | `previous` chains a run's records. At most one active run per drive and per cartridge (today's unique indexes). Register and Expire concern the cartridge, not a run. **The fold never derives a fence from cleaning** (item 11): a run that needs an operator is a separate `FenceRaised(DriveFault)` |
| 22 | AlarmChanged | Alarm, cause scopes | `change` against the alarm generation (0 when never raised). Raise requires absent or cleared; Acknowledge requires raised and unacknowledged, with an envelope actor other than System (the acknowledger is the actor, recorded once); Clear requires raised. `cause` may name any earlier record, using LibraryHistory after replacement |
| 23 | OperationRecorded | Operation | `previous` chains the operation's records. Accepted comes first or not at all. Started follows nothing or Accepted. Cancel records follow a non-terminal state. Finished, Failed, CancelledBeforeDispatch, CompletedAfterCancel and CompletionUnknown are terminal; nothing follows them. The idempotency scope (`actor_fingerprint`, `kind`, `idempotency_key`) is unique: a second Accepted for that scope with a different operation or fingerprint is a `ConflictingDuplicate`. An identical keyed retry yields `Duplicate(original receipt)` while Accepted is latest, `Chain` after Started or another non-terminal successor, or `ClosedOperationReplay { operation }` after the operation closes. B4b's API must answer all three from its projection, treating `Chain` for an identical keyed Accepted as already running and using the operation id for a closed retry because the fold does not retain terminal receipts. Terminal is enforced for chained successors; reuse of a closed unkeyed operation id is not detected in fold state. B4b writers mint fresh ids. **No progress variant**: progress gates nothing in step 4 and stays `Diagnostic` |
| 24 | SessionObserved | Operation(session), Tape, Endpoint | Opened comes first; the other events are terminal. **A write session cannot be expressed**: write sessions are the tape log's (item 5b), and kind 0 is refused. It has no receipts, because durable receipts are tape-log only |
| 25 | WrapMapCached | Tape | `digest` = SHA-256(`descriptors`), whose bytes are today's descriptor serialisation, opaque to the fold. Only digest and calibration_generation are retained per tape; the projector reads descriptor bytes from the source record. `calibration_generation` never decreases per tape. The RAO control store (the third authority) still decides servability; this rebuilds only the SQL cache |
| 26 | RecoveryRecorded | Tape, Operation, prior scopes | **Pending** on `at`. RepairApplied requires `retained_through = at.sequence`. **`AppendRecoveryRequired` does not exist** (item 5c): action 0 is refused. Missing native proof stays pending. `prior` must name an earlier RecoveryRecorded, including one replaced in retained state |
| 32 | Diagnostic | none | Detail keys are strictly ascending and unique (InvalidSchema); a nested value is refused at encode. **The fold validates and ignores it.** `DiagnosticEvent` is B2's own closed enum (below) |

All generation failures use GenerationMismatch. Shape violations use InvalidSchema; pair/type failures use WrongCounterpart or the named rule error. Digest mismatch is scoped TapeContradiction and is checked first; only a matching digest with wrong kind is CounterpartKind. Tag 3 adds mandatory key 4, detail: text <=4096 bytes or null; it is presentation only. Tag 21 is removed from production orchestration in step 5; its historical decoder remains.

### Additional wire conventions

Id is b16; Hash is b32; Text/TextValue is UTF-8 text <=4096 bytes; LabelText is UTF-8 text <=32 bytes. Blob<N> is a CBOR byte string <=N bytes; Bounded<T,N> is a definite array <=N entries. Integer types are unsigned, except Negative, the diagnostic i64 <0. Struct names are mandatory ascending integer-keyed maps. Unit is null. DiagnosticEntry is [text,DiagnosticValue]. AuditActor is {0: kind (System=0, User=1, Service=2), 1: name (null exactly for System)}. Pool watermark fields encode the existing f64 IEEE-754 bits as u64, preserving exact configuration values without CBOR floats; they are not text.

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
