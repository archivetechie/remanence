//! Pure in-memory format/fold contract, including generated bounded-memory replay.
use super::codec::{encode_frame, CodecError, Decoder, Tail, TornKind};
use super::fold::{Fold, GroupDigest, TapeSummary};
use super::payload::{encode_header, encode_member, CatalogValidator, CommittedObjectProjection};
use super::wire::Wire;
use super::*;
use crate::{
    NativeObjectCopyProjectionInput, NativeObjectFileProjectionInput, NativeObjectProjectionInput,
};
use remanence_crc::crc64_xz;
use remanence_parity::{SchemeId, TapeFileKind, TerminalTailComponentKind};
use std::io::{self, Cursor, Read, Seek, SeekFrom};

const UUID: [u8; 16] = [7; 16];
const BLOCK: u32 = 262144;
fn scheme() -> ParityScheme {
    ParityScheme {
        id: SchemeId::new_static("rs-cauchy-gf256-v1"),
        data_blocks_per_stripe: 2,
        parity_blocks_per_stripe: 1,
        stripes_per_neighborhood: 4,
    }
}
fn plain_row(file: u64, kind: TapeFileKind, blocks: u64, lba: u64) -> TapeFileEntry {
    TapeFileEntry {
        tape_file_number: file,
        kind,
        block_count: blocks,
        physical_start_hint: Some(lba),
        object_id: None,
        first_parity_data_ordinal: None,
        epoch_id: None,
        protected_ordinal_start: None,
        protected_ordinal_end_exclusive: None,
        canonical_metadata_hash: None,
        object_recovery_row: None,
    }
}
fn init(parity: bool) -> Record {
    Record::Initialized(Initialized {
        tape_uuid: UUID,
        incarnation: 1,
        group: 1,
        block_size: BLOCK,
        scheme: parity.then(scheme),
        bot: plain_row(0, TapeFileKind::Bootstrap, 1, 0),
    })
}
fn object(file: u64, lba: u64, first: Option<u64>, blocks: u64) -> TapeFileEntry {
    let mut row = plain_row(file, TapeFileKind::Object, blocks, lba);
    row.object_id = Some("o".into());
    row.first_parity_data_ordinal = first;
    row.object_recovery_row = Some(
        ObjectRecoveryRow::plaintext(file, blocks, 0, 1, 1, [2; 32]).with_object_id(b"o".to_vec()),
    );
    row
}
fn header(row: &TapeFileEntry, total: u64) -> CommittedObjectProjection {
    CommittedObjectProjection {
        object: NativeObjectProjectionInput {
            object_id: "o".into(),
            caller_object_id: Some("caller".into()),
            body_format: "raw".into(),
            logical_size_bytes: Some(12),
            content_hash: Some(vec![3; 32]),
            metadata_hash: Some(vec![4; 32]),
            created_at_utc: Some("2026-10-31T00:00:00Z".into()),
        },
        copy: NativeObjectCopyProjectionInput {
            object_id: "o".into(),
            tape_uuid: UUID,
            tape_file_number: row.tape_file_number,
            first_body_lba: 0,
            first_parity_data_ordinal: row.first_parity_data_ordinal,
            protected_until_ordinal: Some(0),
            status: "committed".into(),
            representation: "plaintext".into(),
            recipient_epoch_ids: None,
            metadata_frame_len: None,
            plaintext_digest: Some(vec![5; 32]),
            stored_digest: Some(vec![6; 32]),
        },
        block_size: BLOCK,
        block_count: row.block_count,
        fresh_tape: true,
        total_committed_ordinals: total,
        object_recovery_row: row.object_recovery_row.clone().unwrap(),
    }
}
fn member() -> NativeObjectFileProjectionInput {
    NativeObjectFileProjectionInput {
        object_id: "o".into(),
        file_id: "f".into(),
        path: "path/λ/🦀".into(),
        size_bytes: 12,
        file_sha256: vec![9; 32],
        first_chunk_lba: Some(0),
        chunk_count: 1,
        mtime: Some("123.45".into()),
        executable: Some(true),
    }
}
fn commit(reference: GroupRef, next_file: u64, lba: u64, objects: u64) -> Committed {
    Committed {
        group: reference,
        cursor: CommitCursor { partition: 0, lba },
        protected: 0,
        total: objects * 3,
        check: CheckValue::EMPTY,
        provenance: CheckProvenance::Empty,
        unverified_end: None,
        next_epoch: 0,
        next_file,
        object_count: objects,
        batch_id: [8; 16],
        session_id: Some([9; 16]),
        seal_due: None,
    }
}
#[derive(Default)]
struct Log {
    bytes: Vec<u8>,
    seq: u64,
    group: Option<GroupDigest>,
    members: u64,
}
impl Log {
    fn next(&self) -> u64 {
        self.seq + 1
    }
    fn group_start(&self) -> u64 {
        self.group
            .as_ref()
            .map_or(self.next(), |g| g.reference(0).first_sequence)
    }
    fn reference(&self) -> GroupRef {
        self.group
            .as_ref()
            .cloned()
            .unwrap_or_else(|| GroupDigest::new(self.next(), self.bytes.len() as u64))
            .reference(self.members)
    }
    fn push(&mut self, record: Record, payload: &[u8]) {
        let offset = self.bytes.len();
        self.seq += 1;
        encode_frame(&mut self.bytes, self.seq, &record, payload).unwrap();
        if matches!(
            record,
            Record::Initialized(_) | Record::PayloadContinuation(_)
        ) {
            let mut decoder = Decoder::at(&self.bytes[offset..], self.seq, offset as u64);
            let frame = decoder.next_frame().unwrap().unwrap();
            self.group
                .get_or_insert_with(|| GroupDigest::new(self.seq, offset as u64))
                .push(&frame)
                .unwrap();
        } else {
            self.group = None;
            self.members = 0;
        }
    }
    fn row(&mut self, row: TapeFileEntry) {
        self.push(
            Record::PayloadContinuation(Continuation {
                group: self.group_start(),
                chunk: Chunk::FileRow(row),
            }),
            &[],
        );
    }
    fn catalog(&mut self, row: &TapeFileEntry, total: u64, member_count: usize, chunk_size: usize) {
        let mut bytes = Vec::new();
        encode_header(&mut bytes, &header(row, total)).unwrap();
        for _ in 0..member_count {
            encode_member(&mut bytes, &header(row, total), &member()).unwrap();
        }
        let count = bytes.len().div_ceil(chunk_size);
        for (i, chunk) in bytes.chunks(chunk_size).enumerate() {
            self.push(
                Record::PayloadContinuation(Continuation {
                    group: self.group_start(),
                    chunk: Chunk::Catalog {
                        tape_file_number: row.tape_file_number,
                        ordinal: i as u64,
                        last: i + 1 == count,
                    },
                }),
                chunk,
            );
        }
        self.members += member_count as u64;
    }
    fn first(parity: bool) -> Self {
        let mut log = Self::default();
        log.push(init(parity), &[]);
        let row = object(1, 2, parity.then_some(0), 3);
        log.row(row.clone());
        log.catalog(&row, 3, 2, 31);
        let mut c = commit(log.reference(), 2, 6, 1);
        if parity {
            c.total = 3;
            c.check = CheckValue(42);
            c.provenance = CheckProvenance::OriginalWrite;
        }
        log.push(Record::Committed(c), &[]);
        log
    }
    fn summary(&self) -> TapeSummary {
        fold::replay(self.bytes.as_slice()).unwrap().0
    }
}
fn plan(reference: GroupRef, start_file: u64, start_lba: u64) -> FinalizationPlan {
    FinalizationPlan {
        group: reference,
        trigger: TerminalFinalizationTrigger::NoPendingObjectFits,
        manual: None,
        edition_id: [1; 16],
        edition_sequence: 1,
        edition_digest: [2; 32],
        writer_version: "remanence test".into(),
        write_timestamp: "2026-10-31T00:00:00Z".into(),
        layout: TerminalTailLayout::new(0, BLOCK, start_file, start_lba, 2, 2).unwrap(),
        prefix_end: CommitCursor {
            partition: 0,
            lba: start_lba,
        },
    }
}
fn fault(error: CodecError) -> Fault {
    match error {
        CodecError::Recovery(e) => e.fault,
        other => panic!("not recovery: {other:?}"),
    }
}
fn rewrite_prefix_crc(bytes: &mut [u8]) {
    let crc = crc64_xz(&bytes[..20]);
    bytes[20..28].copy_from_slice(&crc.to_le_bytes());
}

#[test]
fn every_emitted_tag_round_trips_and_binary_fields_are_byte_strings() {
    let row = object(1, 2, Some(0), 3);
    let p = plan(GroupRef::default(), 2, 6);
    let c = commit(GroupRef::default(), 2, 6, 1);
    let records = vec![
        init(true),
        Record::PayloadContinuation(Continuation {
            group: 1,
            chunk: Chunk::FileRow(row.clone()),
        }),
        Record::PayloadContinuation(Continuation {
            group: 1,
            chunk: Chunk::Catalog {
                tape_file_number: 1,
                ordinal: 0,
                last: true,
            },
        }),
        Record::Committed(c.clone()),
        Record::TerminalPrefix {
            decision: 1,
            commit: c.clone(),
        },
        Record::ComponentWritten {
            decision: 1,
            component: 0,
            commit: c,
        },
        Record::Sealed {
            decision: 1,
            cursor: CommitCursor {
                partition: 0,
                lba: 9,
            },
            edition_digest: [4; 32],
        },
        Record::FinalizationDecided(Box::new(p.clone())),
        Record::FinalizationPrepared(Box::new(p)),
        Record::FinalizationPreparationRetired { prepared: 1 },
        Record::FinalizationRecoveryRequired { plan: 1 },
    ];
    for record in records {
        let mut bytes = Vec::new();
        encode_frame(&mut bytes, 1, &record, &[]).unwrap();
        let mut decoder = Decoder::new(bytes.as_slice());
        let frame = decoder.next_frame().unwrap().unwrap();
        assert_eq!(frame.record, record);
        assert_eq!(frame.record.tag(), record.tag());
        let value: ciborium::Value = ciborium::from_reader(frame.structural).unwrap();
        assert_no_number_arrays(&value);
    }
    let mut encoded = Vec::new();
    row.put(&mut encoded).unwrap();
    let value: ciborium::Value = ciborium::from_reader(encoded.as_slice()).unwrap();
    fn assert_no_number_arrays(v: &ciborium::Value) {
        match v {
            ciborium::Value::Array(values) => {
                assert!(
                    values.is_empty()
                        || !values
                            .iter()
                            .all(|v| matches!(v, ciborium::Value::Integer(_)))
                );
                for value in values {
                    assert_no_number_arrays(value);
                }
            }
            ciborium::Value::Map(fields) => {
                for (_, value) in fields {
                    assert_no_number_arrays(value);
                }
            }
            _ => {}
        }
    }
    assert_no_number_arrays(&value);
    assert_no_number_arrays(&ciborium::Value::Array(Vec::new()));
    let mut bytes = Vec::new();
    CheckValue(0x0102030405060708).put(&mut bytes).unwrap();
    assert_eq!(bytes, [0x48, 8, 7, 6, 5, 4, 3, 2, 1]);
    let h = header(&row, 3);
    let mut encoded = Vec::new();
    encode_header(&mut encoded, &h).unwrap();
    assert_no_number_arrays(
        &ciborium::from_reader::<ciborium::Value, _>(encoded.as_slice()).unwrap(),
    );
    assert_eq!(
        CommittedObjectProjection::get(&mut encoded.as_slice()).unwrap(),
        h
    );
    let m = member();
    let mut encoded = Vec::new();
    encode_member(&mut encoded, &h, &m).unwrap();
    assert_eq!(
        NativeObjectFileProjectionInput::get(&mut encoded.as_slice()).unwrap(),
        m
    );
}
#[test]
fn reserved_tags_are_defined_but_refused_and_terminal_prefix_is_distinct() {
    assert_ne!(Tag::TerminalPrefix, Tag::Committed);
    for tag in [
        Tag::FinalizationPlanned,
        Tag::ClosedOut,
        Tag::SessionOpened,
        Tag::SessionEnded,
        Tag::UnprotectedAccepted,
    ] {
        let mut bytes = Vec::new();
        encode_frame(&mut bytes, 1, &init(false), &[]).unwrap();
        bytes[2..4].copy_from_slice(&(tag as u16).to_le_bytes());
        rewrite_prefix_crc(&mut bytes);
        assert_eq!(
            fault(codec::classify_frames(&bytes).unwrap_err()),
            Fault::ReservedTag(tag)
        );
    }
}
#[test]
fn prefix_structural_and_payload_crc_failures_are_recovery_required() {
    let record = Record::PayloadContinuation(Continuation {
        group: 1,
        chunk: Chunk::Catalog {
            tape_file_number: 1,
            ordinal: 0,
            last: true,
        },
    });
    let mut bytes = Vec::new();
    encode_frame(&mut bytes, 1, &record, b"catalog").unwrap();
    let s = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    for (section, index) in [
        (Section::Prefix, 4),
        (Section::Structural, PREFIX_LEN),
        (Section::Payload, PREFIX_LEN + s + 8),
    ] {
        let mut damaged = bytes.clone();
        damaged[index] ^= 0x80;
        assert_row_full_agreement(&damaged);
        let error = codec::classify_frames(&damaged).unwrap_err();
        assert!(error.recovery_required());
        match error {
            CodecError::Recovery(e) => {
                assert_eq!(e.offset, 0);
                assert_eq!(e.fault, Fault::Crc(section));
            }
            _ => unreachable!(),
        }
    }
}
#[test]
fn version_unknown_tag_hostile_lengths_and_sequence_rules() {
    let mut bytes = Vec::new();
    encode_frame(&mut bytes, 1, &init(false), &[]).unwrap();
    let mut altered = bytes.clone();
    altered[..2].copy_from_slice(&2u16.to_le_bytes());
    rewrite_prefix_crc(&mut altered);
    assert_eq!(
        fault(codec::classify_frames(&altered).unwrap_err()),
        Fault::Version(2)
    );
    let mut altered = bytes.clone();
    altered[2..4].copy_from_slice(&999u16.to_le_bytes());
    rewrite_prefix_crc(&mut altered);
    assert_eq!(
        fault(codec::classify_frames(&altered).unwrap_err()),
        Fault::UnknownTag(999)
    );
    let mut altered = bytes.clone();
    altered[4..12].fill(255);
    rewrite_prefix_crc(&mut altered);
    assert_eq!(
        fault(codec::classify_frames(&altered).unwrap_err()),
        Fault::FrameTooLarge
    );
    for seq in [0, 2, 17] {
        let mut altered = bytes.clone();
        altered[12..20].copy_from_slice(&u64::to_le_bytes(seq));
        rewrite_prefix_crc(&mut altered);
        assert_eq!(
            fault(codec::classify_frames(&altered).unwrap_err()),
            Fault::Sequence {
                expected: 1,
                actual: seq
            }
        );
    }
    for seq in [1, 3, 0] {
        let mut altered = bytes.clone();
        encode_frame(&mut altered, seq, &init(false), &[]).unwrap();
        assert_eq!(
            fault(codec::classify_frames(&altered).unwrap_err()),
            Fault::Sequence {
                expected: 2,
                actual: seq
            }
        );
    }
}
#[test]
fn row_reader_skips_nonfinal_payload_even_with_bad_payload_crc() {
    struct Guard {
        inner: Cursor<Vec<u8>>,
        start: u64,
        end: u64,
        reads: usize,
    }
    impl Read for Guard {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let pos = self.inner.position();
            let end = pos + out.len() as u64;
            assert!(
                end <= self.start || pos >= self.end,
                "payload read at {pos}..{end}"
            );
            self.reads += 1;
            self.inner.read(out)
        }
    }
    impl Seek for Guard {
        fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
            self.inner.seek(p)
        }
    }
    let mut bytes = Vec::new();
    let record = Record::PayloadContinuation(Continuation {
        group: 1,
        chunk: Chunk::Catalog {
            tape_file_number: 1,
            ordinal: 0,
            last: true,
        },
    });
    encode_frame(&mut bytes, 1, &record, b"forbidden payload").unwrap();
    let s = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as u64;
    let start = PREFIX_LEN as u64 + s + 8;
    let end = bytes.len() as u64 - 8;
    bytes[start as usize] ^= 1;
    // Final frames are now validated; only healthy non-final rows skip payload.
    encode_frame(&mut bytes, 2, &init(false), &[]).unwrap();
    let guard = Guard {
        inner: Cursor::new(bytes.clone()),
        start,
        end,
        reads: 0,
    };
    let mut decoder = Decoder::new(guard);
    let frame = decoder.next_row_frame().unwrap().unwrap();
    assert_eq!(frame.record, record);
    assert!(frame.payload.is_empty());
    assert_eq!(
        Fold::new().push(&frame).unwrap_err().fault,
        Fault::Invalid("fold requires all section CRCs")
    );
    assert!(decoder.next_row_frame().unwrap().is_some());
    assert!(decoder.next_row_frame().unwrap().is_none());
    assert_eq!(
        fault(codec::classify_frames(&bytes).unwrap_err()),
        Fault::Crc(Section::Payload)
    );
}
#[test]
fn tail_matrix_distinguishes_short_prefix_short_body_zeros_and_corruption() {
    let base = Log::first(false);
    let old = base.summary();
    let mut final_frame = Vec::new();
    let c = commit(
        GroupDigest::new(base.next(), base.bytes.len() as u64).reference(0),
        2,
        6,
        1,
    );
    encode_frame(&mut final_frame, base.next(), &Record::Committed(c), &[]).unwrap();
    for n in 1..PREFIX_LEN {
        let mut bytes = base.bytes.clone();
        bytes.extend_from_slice(&final_frame[..n]);
        assert_row_full_agreement(&bytes);
        let (summary, tail) = fold::replay(bytes.as_slice()).unwrap();
        assert_eq!(summary.last_closed_sequence, old.last_closed_sequence);
        assert_eq!(
            tail,
            Tail::Torn {
                offset: base.bytes.len() as u64,
                kind: TornKind::ShortPrefix
            }
        );
    }
    for n in PREFIX_LEN..final_frame.len() {
        let mut bytes = base.bytes.clone();
        bytes.extend_from_slice(&final_frame[..n]);
        assert_row_full_agreement(&bytes);
        let (s, tail) = fold::replay(bytes.as_slice()).unwrap();
        assert_eq!(s.last_closed_sequence, old.last_closed_sequence);
        assert!(
            matches!(tail, Tail::Torn { offset, kind: TornKind::ShortBody | TornKind::ZeroFilled }
            if offset == base.bytes.len() as u64)
        );
    }
    for n in [1, 27, 28, 8192, 18000] {
        let mut bytes = base.bytes.clone();
        bytes.resize(bytes.len() + n, 0);
        assert_row_full_agreement(&bytes);
        assert_eq!(
            fold::classify(&bytes).unwrap(),
            Tail::Torn {
                offset: base.bytes.len() as u64,
                kind: TornKind::ZeroFilled
            }
        );
        *bytes.last_mut().unwrap() = 1;
        assert_row_full_agreement(&bytes);
        if n == 1 {
            assert_eq!(
                fold::classify(&bytes).unwrap(),
                Tail::Torn {
                    offset: base.bytes.len() as u64,
                    kind: TornKind::ShortPrefix,
                }
            );
        } else {
            assert_eq!(
                fault(fold::classify(&bytes).unwrap_err()),
                Fault::NonZeroAfterZeros {
                    offset: (base.bytes.len() + n - 1) as u64,
                }
            );
        }
    }
    for suffix in [&b"garbage"[..], &[0, 0, 0, 1][..]] {
        let mut bytes = base.bytes.clone();
        bytes.extend_from_slice(suffix);
        assert_row_full_agreement(&bytes);
        if suffix[0] == 0 {
            assert_eq!(
                fault(fold::classify(&bytes).unwrap_err()),
                Fault::NonZeroAfterZeros {
                    offset: (base.bytes.len() + 3) as u64
                }
            );
        } else {
            assert_eq!(
                fault(fold::classify(&bytes).unwrap_err()),
                Fault::Version(u16::from_le_bytes(*b"ga"))
            );
        }
    }
    let mut bytes = base.bytes.clone();
    bytes.extend_from_slice(&final_frame);
    *bytes.last_mut().unwrap() ^= 1;
    assert_row_full_agreement(&bytes);
    assert_eq!(
        fault(fold::classify(&bytes).unwrap_err()),
        Fault::Crc(Section::Payload)
    );
    assert_row_full_agreement(&base.bytes);
    assert_eq!(fold::classify(&base.bytes).unwrap(), Tail::Complete);
}
#[test]
fn initialized_and_first_commit_share_one_authority_group() {
    let mut log = Log::default();
    log.push(init(false), &[]);
    let (s, tail) = fold::replay(log.bytes.as_slice()).unwrap();
    assert!(s.is_fresh());
    assert_eq!(
        tail,
        Tail::Torn {
            offset: 0,
            kind: TornKind::UnclosedGroup {
                zero_filled_at: None
            }
        }
    );
    let c = commit(log.reference(), 1, 2, 0);
    log.push(Record::Committed(c), &[]);
    let s = log.summary();
    assert!(!s.is_fresh());
    assert_eq!(s.tape_uuid, Some(UUID));
    assert_eq!(s.barrier_count, 1);
    assert_eq!(s.last_closed_sequence, 2);
    let base = Log::first(false);
    let s = base.summary();
    assert_eq!(s.object_count, 1);
    assert_eq!(s.next_file, 2);
    assert_eq!(s.cursor.lba, 6);
    assert_eq!(s.barrier_count, 1);
}
#[test]
fn unclosed_catalog_group_is_absent_but_malformed_group_is_recovery() {
    let mut log = Log::first(false);
    let boundary = log.bytes.len();
    let seq = log.seq;
    let row = object(2, 6, None, 3);
    log.row(row.clone());
    log.catalog(&row, row.tape_file_number * 3, 3, 7);
    assert_row_full_agreement(&log.bytes);
    let (s, tail) = fold::replay(log.bytes.as_slice()).unwrap();
    assert_eq!(s.last_closed_sequence, seq);
    assert_eq!(s.object_count, 1);
    assert_eq!(
        tail,
        Tail::Torn {
            offset: boundary as u64,
            kind: TornKind::UnclosedGroup {
                zero_filled_at: None
            }
        }
    );
    log.row(object(4, 10, None, 3));
    assert_eq!(
        fault(fold::classify(&log.bytes).unwrap_err()),
        Fault::Invalid("dense rows or cursor/extent agreement")
    );
}
#[test]
fn second_initialization_terminal_first_and_missing_frames_are_typed() {
    let mut log = Log::first(false);
    log.push(init(false), &[]);
    assert_eq!(
        fault(fold::classify(&log.bytes).unwrap_err()),
        Fault::SecondInitialized
    );
    let terminal = [
        Record::FinalizationDecided(Box::new(plan(GroupRef::default(), 0, 0))),
        Record::TerminalPrefix {
            decision: 1,
            commit: commit(GroupRef::default(), 1, 2, 0),
        },
        Record::ComponentWritten {
            decision: 1,
            component: 0,
            commit: commit(GroupRef::default(), 1, 2, 0),
        },
        Record::Sealed {
            decision: 1,
            cursor: CommitCursor::default(),
            edition_digest: [1; 32],
        },
    ];
    for r in terminal {
        let mut log = Log::default();
        log.push(r.clone(), &[]);
        assert_eq!(
            fault(fold::classify(&log.bytes).unwrap_err()),
            Fault::TerminalFirst
        );
        let mut log = Log::default();
        log.push(init(false), &[]);
        log.push(r, &[]);
        assert_eq!(
            fault(fold::classify(&log.bytes).unwrap_err()),
            Fault::TerminalFirst
        );
    }
    for field in 0..6 {
        let mut log = Log::first(false);
        let mut reference = log.reference();
        match field {
            0 => reference.first_sequence += 1,
            1 => reference.start_offset += 1,
            2 => reference.frame_count += 1,
            3 => reference.byte_count += 1,
            4 => reference.member_count += 1,
            _ => reference.digest[0] ^= 1,
        };
        log.push(Record::Committed(commit(reference, 2, 6, 1)), &[]);
        assert_eq!(
            fault(fold::classify(&log.bytes).unwrap_err()),
            Fault::GroupMismatch
        );
    }
}
#[test]
fn continuation_order_object_bijection_and_closed_group_digest_are_checked() {
    for case in 0..5 {
        let mut log = Log::first(false);
        let row = object(2, 6, None, 3);
        log.row(row.clone());
        if case == 0 {
            log.row(object(3, 10, None, 3));
        } else {
            let mut payload = Vec::new();
            encode_header(&mut payload, &header(&row, 6)).unwrap();
            log.push(
                Record::PayloadContinuation(Continuation {
                    group: if case == 1 { 999 } else { log.group_start() },
                    chunk: Chunk::Catalog {
                        tape_file_number: if case == 2 { 9 } else { 2 },
                        ordinal: if case == 3 { 1 } else { 0 },
                        last: case != 4,
                    },
                }),
                &payload,
            );
            log.push(Record::Committed(commit(log.reference(), 3, 10, 2)), &[]);
        }
        let expected = match case {
            0 => Fault::Invalid("Object without complete catalog"),
            1 => Fault::GroupMismatch,
            2 | 3 => Fault::Invalid("catalog envelope/order"),
            _ => Fault::Invalid("closing incomplete catalog"),
        };
        assert_eq!(
            fault(fold::classify(&log.bytes).unwrap_err()),
            expected,
            "case {case}"
        );
    }
    let mut log = Log::first(false);
    let row = object(2, 6, None, 3);
    log.row(row.clone());
    log.catalog(&row, row.tape_file_number * 3, 0, 11);
    let mut reference = log.reference();
    reference.digest[0] ^= 1;
    log.push(Record::Committed(commit(reference, 3, 10, 2)), &[]);
    assert_eq!(
        fault(fold::classify(&log.bytes).unwrap_err()),
        Fault::GroupMismatch
    );
}
#[test]
fn catalog_crosses_every_byte_boundary_preserves_metadata_and_rejects_bad_utf8() {
    let row = object(1, 2, Some(0), 3);
    let mut bytes = Vec::new();
    encode_header(&mut bytes, &header(&row, 3)).unwrap();
    encode_member(&mut bytes, &header(&row, 3), &member()).unwrap();
    for split in 1..=bytes.len() {
        let mut validator = CatalogValidator::new(&row, UUID, BLOCK, 3).unwrap();
        for chunk in bytes.chunks(split) {
            validator.feed(chunk).unwrap();
        }
        assert_eq!(validator.finish().unwrap(), 1);
        assert!(validator.retained_bytes() < 4096);
    }
    let mut broken = bytes.clone();
    let at = broken.windows(2).position(|w| w == "λ".as_bytes()).unwrap();
    broken[at + 1] = 255;
    let mut validator = CatalogValidator::new(&row, UUID, BLOCK, 3).unwrap();
    assert_eq!(
        validator.feed(&broken).unwrap_err(),
        Fault::Invalid("UTF-8")
    );
    for n in [0, 1, bytes.len() - 1] {
        let mut validator = CatalogValidator::new(&row, UUID, BLOCK, 3).unwrap();
        validator.feed(&bytes[..n]).unwrap();
        assert_eq!(
            validator.finish().unwrap_err(),
            Fault::Invalid("incomplete catalog item")
        );
    }
    for field in 0..5 {
        let mut h = header(&row, 3);
        match field {
            0 => h.object.object_id = "wrong".into(),
            1 => h.copy.tape_uuid = [0; 16],
            2 => h.copy.tape_file_number = 99,
            3 => h.block_count = 4,
            _ => h.object_recovery_row.stored_block_count = 9,
        };
        let mut b = Vec::new();
        h.put(&mut b).unwrap();
        let mut validator = CatalogValidator::new(&row, UUID, BLOCK, 3).unwrap();
        assert_eq!(
            validator.feed(&b).unwrap_err(),
            Fault::Invalid(match field {
                0 => "catalog identity length",
                1 | 4 => "catalog identity mismatch",
                _ => "catalog CBOR schema",
            })
        );
    }
}
#[test]
fn duplicate_keys_nonminimal_integers_arrays_for_hashes_and_trailing_cbor_refused() {
    let mut bytes = Vec::new();
    init(false).tag();
    wire::encode(&init(false), &mut bytes).unwrap();
    let mut wrong = bytes.clone();
    assert_eq!(wrong[0], 0xa6);
    wrong[1] = 1;
    assert_eq!(
        wire::decode(Tag::Initialized, &wrong).unwrap_err(),
        Fault::Invalid("CBOR schema")
    );
    let mut wrong = bytes.clone();
    wrong.splice(1..2, [0x18, 0]);
    assert_eq!(
        wire::decode(Tag::Initialized, &wrong).unwrap_err(),
        Fault::Invalid("CBOR schema")
    );
    let mut wrong = bytes.clone();
    wrong.push(0);
    assert_eq!(
        wire::decode(Tag::Initialized, &wrong).unwrap_err(),
        Fault::Invalid("trailing structural bytes")
    );
    let mut wrong = bytes.clone();
    assert_eq!(wrong[2], 0x50);
    wrong[2] = 0x90;
    assert_eq!(
        wire::decode(Tag::Initialized, &wrong).unwrap_err(),
        Fault::Invalid("CBOR schema")
    );
    let mut wrong = bytes;
    wrong[2] = 0x5b;
    wrong.splice(3..3, [255; 8]);
    assert_eq!(
        wire::decode(Tag::Initialized, &wrong).unwrap_err(),
        Fault::Invalid("CBOR schema")
    );
}
#[test]
fn unverified_rebuild_is_retained_until_coverage_and_seal_reason_is_retained() {
    let mut log = Log::first(true);
    let mut c = commit(log.reference(), 2, 6, 1);
    c.total = 3;
    c.check = CheckValue(43);
    c.provenance = CheckProvenance::UnverifiedRebuild;
    c.unverified_end = Some(3);
    c.seal_due = Some(SealDueReason::HardwareEarlyWarning);
    log.push(Record::Committed(c.clone()), &[]);
    let s = log.summary();
    assert_eq!(s.provenance, CheckProvenance::UnverifiedRebuild);
    assert_eq!(s.seal_due, Some(SealDueReason::HardwareEarlyWarning));
    assert_eq!(s.uncovered.len(), 1);
    let prefix = log.bytes.clone();
    c.group = log.reference();
    c.provenance = CheckProvenance::OriginalWrite;
    c.unverified_end = None;
    log.push(Record::Committed(c.clone()), &[]);
    assert_eq!(
        fault(fold::classify(&log.bytes).unwrap_err()),
        Fault::Invalid("unverified rebuild promoted")
    );
    log.bytes = prefix;
    log.seq -= 1;
    c.group = log.reference();
    c.provenance = CheckProvenance::UnverifiedRebuild;
    c.unverified_end = Some(3);
    c.seal_due = None;
    log.push(Record::Committed(c), &[]);
    assert_eq!(
        fault(fold::classify(&log.bytes).unwrap_err()),
        Fault::Invalid("lost SealDueReason")
    );
}
fn sidecar(file: u64, lba: u64, epoch: u64, start: u64, end: u64) -> TapeFileEntry {
    let mut row = plain_row(file, TapeFileKind::ParitySidecar, 2, lba);
    row.epoch_id = Some(epoch);
    row.protected_ordinal_start = Some(start);
    row.protected_ordinal_end_exclusive = Some(end);
    row.canonical_metadata_hash = Some([4; 32]);
    row
}
#[test]
fn actual_sidecars_increment_epoch_and_empty_check_resets_provenance() {
    let mut log = Log::first(true);
    let mut c = commit(log.reference(), 2, 6, 1);
    c.total = 3;
    c.check = CheckValue(42);
    c.provenance = CheckProvenance::UnverifiedRebuild;
    c.unverified_end = Some(3);
    log.push(Record::Committed(c), &[]);
    log.row(sidecar(2, 6, 0, 0, 3));
    let mut c = commit(log.reference(), 3, 9, 1);
    c.total = 3;
    c.protected = 3;
    c.next_epoch = 1;
    log.push(Record::Committed(c), &[]);
    let s = log.summary();
    assert_eq!(s.next_epoch, 1);
    assert_eq!(s.provenance, CheckProvenance::Empty);
    assert_eq!(s.check, CheckValue::EMPTY);
    assert!(s.uncovered.is_empty());
}
#[test]
fn combined_decision_plan_closes_admission_and_metadata_preserves_media_boundary() {
    let mut log = Log::first(false);
    let prior = log.summary();
    let decision = log.next();
    let p = plan(log.reference(), 2, 6);
    log.push(Record::FinalizationDecided(Box::new(p)), &[]);
    let s = log.summary();
    let plan = s.finalization.unwrap();
    assert!(!plan.prepared);
    assert_eq!(plan.sequence, decision);
    assert_eq!(s.last_media_commit, prior.last_media_commit);
    assert_eq!(s.cursor, prior.cursor);
    assert_eq!(s.barrier_count, prior.barrier_count);
    let c = commit(log.reference(), 2, 6, 1);
    log.push(Record::Committed(c), &[]);
    assert_eq!(
        fault(fold::classify(&log.bytes).unwrap_err()),
        Fault::Invalid("ordinary commit after finalization admission")
    );
}
#[test]
fn five_exact_components_then_sealed_and_no_records_afterward() {
    let mut log = Log::first(false);
    let p = plan(log.reference(), 2, 6);
    let decision = log.next();
    let layout = p.layout;
    log.push(Record::FinalizationDecided(Box::new(p)), &[]);
    for (i, component) in layout.components.iter().enumerate() {
        let kind = match component.kind {
            TerminalTailComponentKind::TapeIndexReplica => TapeFileKind::TapeIndexReplica,
            TerminalTailComponentKind::IndexSeparationExtent => TapeFileKind::IndexSeparationExtent,
        };
        log.row(plain_row(
            component.planned_tape_file_number,
            kind,
            component.record_count,
            component.planned_start_lba,
        ));
        let c = commit(
            log.reference(),
            component.planned_tape_file_number + 1,
            component.planned_start_lba + component.record_count + 1,
            1,
        );
        log.push(
            Record::ComponentWritten {
                decision,
                component: i as u8,
                commit: c,
            },
            &[],
        );
        assert_eq!(
            log.summary().finalization.unwrap().components_written,
            i as u8 + 1
        );
    }
    let cursor = CommitCursor {
        partition: 0,
        lba: layout.expected_eod_lba,
    };
    log.push(
        Record::Sealed {
            decision,
            cursor,
            edition_digest: [2; 32],
        },
        &[],
    );
    assert!(log.summary().sealed);
    log.push(Record::FinalizationRecoveryRequired { plan: decision }, &[]);
    assert_eq!(
        fault(fold::classify(&log.bytes).unwrap_err()),
        Fault::Invalid("record after seal")
    );
}
#[test]
fn premature_seal_wrong_component_and_wrong_plan_are_refused() {
    for case in 0..3 {
        let mut log = Log::first(false);
        let p = plan(log.reference(), 2, 6);
        let decision = log.next();
        log.push(Record::FinalizationDecided(Box::new(p)), &[]);
        if case == 0 {
            log.push(
                Record::Sealed {
                    decision,
                    cursor: CommitCursor {
                        partition: 0,
                        lba: 6,
                    },
                    edition_digest: [2; 32],
                },
                &[],
            );
        } else {
            log.row(plain_row(2, TapeFileKind::TapeIndexReplica, 2, 6));
            let c = commit(log.reference(), 3, 9, 1);
            log.push(
                Record::ComponentWritten {
                    decision: if case == 1 { 999 } else { decision },
                    component: if case == 2 { 1 } else { 0 },
                    commit: c,
                },
                &[],
            );
        }
        assert_eq!(
            fault(fold::classify(&log.bytes).unwrap_err()),
            Fault::Invalid(match case {
                0 => "seal before replica C or plan mismatch",
                1 => "no accepted writable plan",
                _ => "terminal component transition",
            })
        );
    }
}

#[test]
fn a_single_catalog_field_larger_than_a_frame_is_refused_before_consumption() {
    let row = object(1, 2, None, 3);
    let mut encoded = Vec::new();
    encode_header(&mut encoded, &header(&row, 3)).unwrap();
    let start = encoded.windows(6).position(|w| w == b"caller").unwrap();
    let mut head = Vec::new();
    wire::head(&mut head, 3, MAX_FRAME_BODY as u64 + 1).unwrap();
    let mut validator = CatalogValidator::new(&row, UUID, BLOCK, 3).unwrap();
    validator.feed(&encoded[..start - 1]).unwrap();
    assert_eq!(validator.feed(&head).unwrap_err(), Fault::FrameTooLarge);
    // Typed decoding must refuse the same declared length before reading data.
    use wire::Wire;
    let mut reader = std::io::Cursor::new(head.clone());
    assert_eq!(
        String::get(&mut reader).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    assert_eq!(reader.position(), head.len() as u64);
    let mut payload = encoded[..start - 1].to_vec();
    payload.extend_from_slice(&head);
    let mut log = Log::default();
    log.push(init(false), &[]);
    log.row(row);
    log.push(
        Record::PayloadContinuation(Continuation {
            group: 1,
            chunk: Chunk::Catalog {
                tape_file_number: 1,
                ordinal: 0,
                last: true,
            },
        }),
        &payload,
    );
    // All frame CRCs are correct; the incremental fold rejects the scalar length
    // before a typed visitor can consume the remainder as a single field.
    assert_eq!(
        fault(fold::replay(log.bytes.as_slice()).unwrap_err()),
        Fault::FrameTooLarge
    );
}

fn directory(row: &TapeFileEntry) -> SidecarEpochDirectoryEntry {
    SidecarEpochDirectoryEntry {
        tape_file_number: row.tape_file_number,
        epoch_id: row.epoch_id.unwrap(),
        protected_ordinal_start: row.protected_ordinal_start.unwrap(),
        protected_ordinal_end_exclusive: row.protected_ordinal_end_exclusive.unwrap(),
        sidecar_total_block_count: row.block_count,
        sidecar_header_block_count: 1,
        parity_shard_block_count: 2,
        canonical_metadata_hash: row.canonical_metadata_hash.unwrap(),
        flags: 1,
    }
}
#[test]
fn terminal_prefix_has_its_own_exact_planned_rows_and_streamed_directory() {
    let mut log = Log::first(true);
    let side = sidecar(2, 6, 0, 0, 3);
    log.row(side.clone());
    let mut map = plain_row(3, TapeFileKind::ParityMap, 1, 9);
    map.canonical_metadata_hash = Some([5; 32]);
    log.row(map.clone());
    let dir = directory(&side);
    log.push(
        Record::PayloadContinuation(Continuation {
            group: log.group_start(),
            chunk: Chunk::PlanDirectory(dir),
        }),
        &[],
    );
    let p = plan(log.reference(), 4, 11);
    let decision = log.next();
    log.push(Record::FinalizationDecided(Box::new(p)), &[]);
    let s = log.summary();
    assert_eq!(s.next_file, 2);
    assert_eq!(s.protected, 0);
    assert!(!s.finalization.unwrap().prefix_written);
    log.row(side);
    log.row(map);
    let mut c = commit(log.reference(), 4, 11, 1);
    c.protected = 3;
    c.total = 3;
    c.next_epoch = 1;
    log.push(
        Record::TerminalPrefix {
            decision,
            commit: c,
        },
        &[],
    );
    let s = log.summary();
    assert_eq!(s.protected, 3);
    assert_eq!(s.next_epoch, 1);
    assert!(s.finalization.unwrap().prefix_written);
}
fn manual_plan(reference: GroupRef) -> FinalizationPlan {
    let mut p = plan(reference, 2, 6);
    p.trigger = TerminalFinalizationTrigger::OperatorCloseOut;
    p.manual = Some(crate::ManualTerminalFinalizationIdentity {
        operation_id: [1; 16],
        operation_kind: "finalize".into(),
        actor_fingerprint: "actor".into(),
        idempotency_key: [2; 16],
        request_fingerprint: [3; 32],
        assigned_pool_id: Some("pool".into()),
        expected_pool_id: Some("pool".into()),
        assignment_generation: 1,
        reason: "test".into(),
    });
    p
}
#[test]
fn compatibility_preparation_retirement_recovery_and_acceptance() {
    for retire in [false, true] {
        let mut log = Log::first(false);
        let prepared = log.next();
        let p = manual_plan(log.reference());
        log.push(Record::FinalizationPrepared(Box::new(p)), &[]);
        assert!(log.summary().finalization.unwrap().prepared);
        if retire {
            log.push(Record::FinalizationPreparationRetired { prepared }, &[]);
            assert!(log.summary().finalization.is_none());
        } else {
            let p = manual_plan(log.reference());
            let accepted = log.next();
            log.push(Record::FinalizationDecided(Box::new(p)), &[]);
            assert!(!log.summary().finalization.unwrap().prepared);
            log.push(Record::FinalizationRecoveryRequired { plan: accepted }, &[]);
            assert!(log.summary().finalization.unwrap().recovery_required);
        }
    }
    let mut log = Log::first(false);
    let p = plan(log.reference(), 2, 6);
    log.push(Record::FinalizationPrepared(Box::new(p)), &[]);
    assert_eq!(
        fault(fold::classify(&log.bytes).unwrap_err()),
        Fault::Invalid("automatic plan cannot be provisional")
    );
}
#[test]
fn fold_rejects_dense_row_counter_parity_and_geometry_contradictions() {
    for case in 0..7 {
        let mut log = Log::first(true);
        let mut row = object(2, 6, Some(3), 3);
        match case {
            0 => row.tape_file_number = 3,
            1 => row.physical_start_hint = Some(7),
            2 => row.first_parity_data_ordinal = Some(4),
            3 => row.object_recovery_row = None,
            _ => {}
        }
        log.row(row.clone());
        if case >= 4 {
            log.catalog(&row, 6, 0, 53);
            let mut c = commit(log.reference(), 3, 10, 2);
            c.total = 6;
            c.check = CheckValue(5);
            c.provenance = CheckProvenance::OriginalWrite;
            match case {
                4 => c.cursor.lba = 11,
                5 => c.total = 7,
                _ => c.next_epoch = 1,
            };
            log.push(Record::Committed(c), &[]);
        }
        assert_eq!(
            fault(fold::classify(&log.bytes).unwrap_err()),
            Fault::Invalid(match case {
                0 | 1 => "dense rows or cursor/extent agreement",
                2 => "Object data ordinal",
                3 => "Object recovery row missing",
                _ => "commit counters/cursor",
            }),
            "case {case}"
        );
    }
    for case in 0..3 {
        let mut log = Log::first(true);
        let mut row = sidecar(2, 6, 0, 0, 3);
        match case {
            0 => row.epoch_id = Some(1),
            1 => row.protected_ordinal_start = Some(1),
            _ => row.protected_ordinal_end_exclusive = Some(4),
        };
        log.row(row);
        assert_eq!(
            fault(fold::classify(&log.bytes).unwrap_err()),
            Fault::Invalid("sidecar coverage/epoch")
        );
    }
}
#[test]
fn fold_independently_refuses_sequence_gaps_and_is_poisoned_after_error() {
    let mut bytes = Vec::new();
    encode_frame(&mut bytes, 2, &init(false), &[]).unwrap();
    let mut decoder = Decoder::at(bytes.as_slice(), 2, 0);
    let frame = decoder.next_frame().unwrap().unwrap();
    let mut fold = Fold::default();
    let error = fold.push(&frame).unwrap_err();
    assert_eq!(
        error.fault,
        Fault::Sequence {
            expected: 1,
            actual: 2
        }
    );
    assert_eq!(
        fold.push(&frame).unwrap_err().fault,
        Fault::Invalid("fold poisoned")
    );
    assert!(fold.summary().is_fresh());
}
#[test]
fn maximum_frame_bound_is_checked_before_encoder_or_decoder_allocation() {
    let payload = vec![0; MAX_FRAME_BODY];
    let mut bytes = Vec::new();
    let result = encode_frame(&mut bytes, 1, &init(false), &payload);
    assert!(matches!(
        result,
        Err(CodecError::EncodeSchema(Fault::FrameTooLarge))
    ));
    assert!(bytes.is_empty());
    let mut prefix = codec::prefix_bytes(VERSION, Tag::Committed, u32::MAX, 1, 1).to_vec();
    prefix.extend_from_slice(&crc64_xz(&prefix).to_le_bytes());
    let mut decoder = Decoder::new(prefix.as_slice());
    assert_eq!(
        fault(decoder.next_frame().unwrap_err()),
        Fault::FrameTooLarge
    );
    assert_eq!(decoder.retained_buffer_bytes(), 0);
}

#[test]
fn encrypted_catalog_uses_binary_epoch_ids_and_preserves_every_copy_field() {
    let mut row = object(1, 2, None, 3);
    row.object_recovery_row = Some(
        ObjectRecoveryRow::encrypted(1, 3, vec![[0xab; 16]], 17, 1191)
            .with_object_id(b"o".to_vec()),
    );
    let mut h = header(&row, 0);
    h.copy.representation = "encrypted".into();
    h.copy.recipient_epoch_ids = Some(vec!["abababababababababababababababab".into()]);
    h.copy.metadata_frame_len = Some(17);
    let mut bytes = Vec::new();
    encode_header(&mut bytes, &h).unwrap();
    assert!(!bytes
        .windows(32)
        .any(|w| w == b"abababababababababababababababab"));
    assert!(bytes
        .windows(17)
        .any(|w| w == [&[0x50], &[0xab; 16][..]].concat()));
    assert_eq!(
        CommittedObjectProjection::get(&mut bytes.as_slice()).unwrap(),
        h
    );
    let mut validator = CatalogValidator::new(&row, UUID, BLOCK, 0).unwrap();
    for byte in &bytes {
        validator.feed(&[*byte]).unwrap();
    }
    assert_eq!(validator.finish().unwrap(), 0);
}
#[test]
fn accepted_preparation_cannot_change_operator_evidence_and_progress_clears_recovery() {
    let mut log = Log::first(false);
    let p = manual_plan(log.reference());
    log.push(Record::FinalizationPrepared(Box::new(p)), &[]);
    let mut changed = manual_plan(log.reference());
    changed.manual.as_mut().unwrap().reason = "different request".into();
    log.push(Record::FinalizationDecided(Box::new(changed)), &[]);
    assert_eq!(
        fault(fold::classify(&log.bytes).unwrap_err()),
        Fault::Invalid("conflicting finalization plan")
    );
    let mut log = Log::first(false);
    let decision = log.next();
    let p = plan(log.reference(), 2, 6);
    log.push(Record::FinalizationDecided(Box::new(p)), &[]);
    log.push(Record::FinalizationRecoveryRequired { plan: decision }, &[]);
    assert!(log.summary().finalization.unwrap().recovery_required);
    log.row(plain_row(2, TapeFileKind::TapeIndexReplica, 2, 6));
    let c = commit(log.reference(), 3, 9, 1);
    log.push(
        Record::ComponentWritten {
            decision,
            component: 0,
            commit: c,
        },
        &[],
    );
    let state = log.summary().finalization.unwrap();
    assert_eq!(state.components_written, 1);
    assert!(!state.recovery_required);
}
#[test]
fn pending_groups_do_not_clone_old_extents_and_commit_splices_new_suffix() {
    let mut log = Log::first(true);
    let old = log.summary();
    let row = object(2, 6, Some(3), 2);
    log.row(row.clone());
    log.catalog(&row, 5, 0, 17);
    let mut decoder = Decoder::new(log.bytes.as_slice());
    let mut fold = Fold::new();
    while let Some(frame) = decoder.next_frame().unwrap() {
        fold.push(&frame).unwrap();
    }
    assert_eq!(fold.summary().uncovered, old.uncovered);
    assert!(fold.retained_extent_capacity() <= 16);
    let mut c = commit(log.reference(), 3, 9, 2);
    c.total = 5;
    c.provenance = CheckProvenance::OriginalWrite;
    c.check = CheckValue(98);
    log.push(Record::Committed(c), &[]);
    let state = log.summary();
    assert_eq!(state.uncovered.len(), 2);
    assert_eq!(state.uncovered[0], old.uncovered[0]);
    assert_eq!(state.uncovered[1].first, 3);
    assert_eq!(state.uncovered[1].end, 5);
    let mut c = commit(log.reference(), 3, 9, 2);
    c.total = 5;
    c.provenance = CheckProvenance::OriginalWrite;
    c.check = CheckValue(98);
    log.push(Record::Committed(c), &[]);
    assert_eq!(log.summary().uncovered, state.uncovered);
}
#[test]
fn many_objects_and_sidecar_cycles_keep_only_epoch_sized_extents() {
    let mut log = Log::default();
    log.push(init(true), &[]);
    let mut file = 1;
    let mut lba = 2;
    let mut total = 0;
    let mut protected = 0;
    let mut epoch = 0;
    for objects in 1..=120 {
        let row = object(file, lba, Some(total), 1);
        log.row(row.clone());
        file += 1;
        lba += 2;
        total += 1;
        log.catalog(&row, total, 0, 4096);
        if total - protected == 8 {
            log.row(sidecar(file, lba, epoch, protected, total));
            file += 1;
            lba += 3;
            protected = total;
            epoch += 1;
        }
        let mut c = commit(log.reference(), file, lba, objects);
        c.total = total;
        c.protected = protected;
        c.next_epoch = epoch;
        if total > protected {
            c.provenance = CheckProvenance::OriginalWrite;
            c.check = CheckValue(total);
        }
        log.push(Record::Committed(c), &[]);
    }
    let mut decoder = Decoder::new(log.bytes.as_slice());
    let mut fold = Fold::new();
    while let Some(frame) = decoder.next_frame().unwrap() {
        fold.push(&frame).unwrap();
        assert!(fold.summary().uncovered.len() < 8);
        assert!(fold.retained_extent_capacity() <= 32);
    }
    let (s, tail) = fold.finish(decoder.tail()).unwrap();
    assert_eq!(tail, Tail::Complete);
    assert_eq!(s.total, 120);
    assert_eq!(s.next_epoch, 15);
    assert!(s.uncovered.is_empty());
}
#[test]
fn valid_crc_does_not_hide_malformed_catalog_or_bad_closed_member_count() {
    let mut log = Log::first(false);
    let row = object(2, 6, None, 3);
    log.row(row.clone());
    let mut bytes = Vec::new();
    encode_header(&mut bytes, &header(&row, 6)).unwrap();
    bytes.push(0xff);
    log.push(
        Record::PayloadContinuation(Continuation {
            group: log.group_start(),
            chunk: Chunk::Catalog {
                tape_file_number: 2,
                ordinal: 0,
                last: true,
            },
        }),
        &bytes,
    );
    assert_eq!(
        fault(fold::classify(&log.bytes).unwrap_err()),
        Fault::Invalid("indefinite/reserved CBOR")
    );
    let mut log = Log::first(false);
    let row = object(2, 6, None, 3);
    log.row(row.clone());
    log.catalog(&row, row.tape_file_number * 3, 2, 19);
    let mut reference = log.reference();
    reference.member_count = 3;
    log.push(Record::Committed(commit(reference, 3, 10, 2)), &[]);
    assert_eq!(
        fault(fold::classify(&log.bytes).unwrap_err()),
        Fault::GroupMismatch
    );
}

#[test]
fn plan_requires_parity_map_exactly_when_directory_is_present() {
    let mut log = Log::first(true);
    let side = sidecar(2, 6, 0, 0, 3);
    log.row(side.clone());
    log.push(
        Record::PayloadContinuation(Continuation {
            group: log.group_start(),
            chunk: Chunk::PlanDirectory(directory(&side)),
        }),
        &[],
    );
    let p = plan(log.reference(), 3, 9);
    log.push(Record::FinalizationDecided(Box::new(p)), &[]);
    assert_eq!(
        fault(fold::classify(&log.bytes).unwrap_err()),
        Fault::Invalid("ParityMap presence/directory")
    );

    let mut log = Log::default();
    log.push(init(true), &[]);
    log.push(Record::Committed(commit(log.reference(), 1, 2, 0)), &[]);
    let mut map = plain_row(1, TapeFileKind::ParityMap, 1, 2);
    map.canonical_metadata_hash = Some([1; 32]);
    log.row(map);
    let p = plan(log.reference(), 2, 4);
    log.push(Record::FinalizationDecided(Box::new(p)), &[]);
    assert_eq!(
        fault(fold::classify(&log.bytes).unwrap_err()),
        Fault::Invalid("ParityMap presence/directory")
    );
}
fn rebuilt_then_fresh() -> Log {
    let mut log = Log::first(true);
    let mut c = commit(log.reference(), 2, 6, 1);
    c.total = 3;
    c.check = CheckValue(43);
    c.provenance = CheckProvenance::UnverifiedRebuild;
    c.unverified_end = Some(3);
    log.push(Record::Committed(c), &[]); // C1: rebuilt [0,3).
    let row = object(2, 6, Some(3), 2);
    log.row(row.clone());
    log.catalog(&row, 5, 0, 17);
    let mut c = commit(log.reference(), 3, 9, 2);
    c.total = 5;
    c.check = CheckValue(45);
    c.provenance = CheckProvenance::UnverifiedRebuild;
    c.unverified_end = Some(3);
    log.push(Record::Committed(c), &[]); // C2: fresh [3,5), unverified end stays 3.
    log
}
#[test]
fn original_write_resumes_when_w_covers_rebuilt_end_not_last_t() {
    let mut log = rebuilt_then_fresh();
    let prior = log.summary();
    assert_eq!(prior.total, 5);
    assert_eq!(prior.unverified_end, Some(3));
    log.row(sidecar(3, 9, 0, 0, 4));
    let mut c = commit(log.reference(), 4, 12, 2);
    c.protected = 4;
    c.total = 5;
    c.next_epoch = 1;
    c.check = CheckValue(5);
    c.provenance = CheckProvenance::OriginalWrite;
    log.push(Record::Committed(c), &[]); // C3: W=4, T=5; only fresh data remains.
    let s = log.summary();
    assert_eq!(s.protected, 4);
    assert_eq!(s.total, 5);
    assert_eq!(s.unverified_end, None);
    assert_eq!(s.provenance, CheckProvenance::OriginalWrite);
}
#[test]
fn unverified_end_cannot_disappear_or_shrink_before_coverage() {
    for marker in [None, Some(2)] {
        let mut log = rebuilt_then_fresh();
        log.row(sidecar(3, 9, 0, 0, 1));
        let mut c = commit(log.reference(), 4, 12, 2);
        c.protected = 1;
        c.total = 5;
        c.next_epoch = 1;
        c.check = CheckValue(5);
        c.unverified_end = marker;
        c.provenance = if marker.is_some() {
            CheckProvenance::UnverifiedRebuild
        } else {
            CheckProvenance::OriginalWrite
        };
        log.push(Record::Committed(c), &[]);
        assert_eq!(
            fault(fold::classify(&log.bytes).unwrap_err()),
            Fault::Invalid("unverified rebuild promoted")
        );
    }
    for (provenance, marker) in [
        (CheckProvenance::OriginalWrite, Some(3)),
        (CheckProvenance::UnverifiedRebuild, None),
        (CheckProvenance::UnverifiedRebuild, Some(6)),
        (CheckProvenance::UnverifiedRebuild, Some(0)),
    ] {
        let mut log = rebuilt_then_fresh();
        let mut c = commit(log.reference(), 3, 9, 2);
        c.total = 5;
        c.check = CheckValue(5);
        c.provenance = provenance;
        c.unverified_end = marker;
        log.push(Record::Committed(c), &[]);
        assert_eq!(
            fault(fold::classify(&log.bytes).unwrap_err()),
            Fault::Invalid("unverified range/provenance")
        );
    }
}
#[test]
fn seal_due_reason_can_change_but_cannot_clear_or_admit_new_object() {
    for (first, next) in [
        (
            SealDueReason::HardwareEarlyWarning,
            SealDueReason::ReachedLowWatermark,
        ),
        (
            SealDueReason::ReachedLowWatermark,
            SealDueReason::HardwareEarlyWarning,
        ),
    ] {
        let mut log = Log::first(false);
        let mut c = commit(log.reference(), 2, 6, 1);
        c.seal_due = Some(first);
        log.push(Record::Committed(c), &[]);
        let mut c = commit(log.reference(), 2, 6, 1);
        c.seal_due = Some(next);
        log.push(Record::Committed(c), &[]);
        assert_eq!(log.summary().seal_due, Some(next));
        let boundary = log.bytes.len();
        log.push(Record::Committed(commit(log.reference(), 2, 6, 1)), &[]);
        assert_eq!(
            fault(fold::classify(&log.bytes).unwrap_err()),
            Fault::Invalid("lost SealDueReason")
        );
        log.bytes.truncate(boundary);
        log.seq -= 1;
        log.row(object(2, 6, None, 3));
        assert_eq!(
            fault(fold::classify(&log.bytes).unwrap_err()),
            Fault::Invalid("Object after seal due")
        );
    }
}
#[test]
fn preflight_checks_one_closed_group_without_changing_or_poisoning_fold() {
    let mut log = Log::first(true);
    let boundary = log.bytes.len();
    let mut fold = Fold::new();
    let mut decoder = Decoder::new(log.bytes.as_slice());
    while let Some(frame) = decoder.next_frame().unwrap() {
        fold.push(&frame).unwrap();
    }
    let before = format!("{:?}", fold.summary());
    let mut c = commit(log.reference(), 2, 6, 1);
    c.total = 3;
    c.check = CheckValue(42);
    c.provenance = CheckProvenance::OriginalWrite;
    let mut invalid = c.clone();
    invalid.total = 2;
    let mut bad = Vec::new();
    encode_frame(&mut bad, log.next(), &Record::Committed(invalid), &[]).unwrap();
    assert_eq!(
        fault(fold.preflight(bad.as_slice()).unwrap_err()),
        Fault::Invalid("commit counters/cursor")
    );
    log.push(Record::Committed(c), &[]);
    fold.preflight(&log.bytes[boundary..]).unwrap();
    assert_eq!(format!("{:?}", fold.summary()), before);
    assert_eq!(
        fault(
            fold.preflight(&log.bytes[boundary..log.bytes.len() - 1])
                .unwrap_err()
        ),
        Fault::Invalid("candidate must contain one closed group")
    );
    assert_eq!(
        fault(fold.preflight(&[][..]).unwrap_err()),
        Fault::Invalid("candidate must contain one closed group")
    );
    let first_end = log.bytes.len();
    let mut c = commit(log.reference(), 2, 6, 1);
    c.total = 3;
    c.check = CheckValue(42);
    c.provenance = CheckProvenance::OriginalWrite;
    log.push(Record::Committed(c), &[]);
    assert_eq!(
        fault(fold.preflight(&log.bytes[boundary..]).unwrap_err()),
        Fault::Invalid("candidate must contain one closed group")
    );
    // The successful candidate is still accepted by the original live fold.
    let mut decoder = Decoder::at(
        &log.bytes[boundary..first_end],
        fold.summary().last_closed_sequence + 1,
        boundary as u64,
    );
    fold.push(&decoder.next_frame().unwrap().unwrap()).unwrap();
    assert_eq!(fold.summary().barrier_count, 2);
    assert_eq!(fold.summary().uncovered.len(), 1);
    Fold::new()
        .preflight(Log::first(false).bytes.as_slice())
        .unwrap();
}
#[test]
fn encode_rejects_bad_catalog_digest_identity_and_recipient_bounds_before_writing() {
    let row = object(1, 2, None, 3);
    let good = header(&row, 0);
    for slot in 0..4 {
        for len in [0, 31, 33] {
            let mut h = good.clone();
            let field = match slot {
                0 => &mut h.object.content_hash,
                1 => &mut h.object.metadata_hash,
                2 => &mut h.copy.plaintext_digest,
                _ => &mut h.copy.stored_digest,
            };
            *field = Some(vec![1; len]);
            let mut bytes = Vec::new();
            assert!(matches!(
                encode_header(&mut bytes, &h),
                Err(CodecError::EncodeSchema(Fault::Invalid(
                    "catalog digest length"
                )))
            ));
            assert!(bytes.is_empty());
        }
    }
    let mut m = member();
    m.object_id = "other".into();
    let mut bytes = Vec::new();
    assert!(matches!(
        encode_member(&mut bytes, &good, &m),
        Err(CodecError::EncodeSchema(Fault::Invalid(
            "catalog member identity"
        )))
    ));
    assert!(bytes.is_empty());
    m = member();
    m.file_sha256.clear();
    assert!(matches!(
        encode_member(&mut bytes, &good, &m),
        Err(CodecError::EncodeSchema(Fault::Invalid(
            "catalog digest length"
        )))
    ));
    assert!(bytes.is_empty());
    let mut h = good.clone();
    h.copy.recipient_epoch_ids = Some(vec!["abababababababababababababababab".into(); 9]);
    assert!(matches!(
        encode_header(&mut bytes, &h),
        Err(CodecError::EncodeSchema(Fault::Invalid(
            "catalog encode schema"
        )))
    ));
    assert!(bytes.is_empty());
    let mut row = row;
    row.object_recovery_row = Some(
        ObjectRecoveryRow::encrypted(1, 3, vec![[1; 16]; 9], 17, 1191)
            .with_object_id(b"o".to_vec()),
    );
    let record = Record::PayloadContinuation(Continuation {
        group: 1,
        chunk: Chunk::FileRow(row),
    });
    assert!(matches!(
        encode_frame(&mut bytes, 1, &record, &[]),
        Err(CodecError::EncodeSchema(Fault::Invalid(
            "CBOR encode schema"
        )))
    ));
    assert!(bytes.is_empty());
    let ids = vec![[1; 16]; 8];
    ids.put(&mut bytes).unwrap();
    assert_eq!(Vec::<[u8; 16]>::get(&mut bytes.as_slice()).unwrap(), ids);
    struct Broken;
    impl io::Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    assert!(
        matches!(encode_header(&mut Broken,&good),Err(CodecError::Io(e)) if e.kind()==io::ErrorKind::BrokenPipe)
    );
}
#[test]
fn zero_extension_inside_final_frame_is_torn_only_through_eof() {
    let log = Log::first(false);
    let offset = log.bytes.len();
    let mut frame = Vec::new();
    encode_frame(
        &mut frame,
        log.next(),
        &Record::Committed(commit(log.reference(), 2, 6, 1)),
        &[],
    )
    .unwrap();
    let structural = u32::from_le_bytes(frame[4..8].try_into().unwrap()) as usize;
    for (start, end) in [
        (PREFIX_LEN, PREFIX_LEN + 5),
        (PREFIX_LEN, frame.len()),
        (PREFIX_LEN, frame.len() + 8193),
        (PREFIX_LEN + structural / 2, frame.len()),
        (PREFIX_LEN + structural / 2, frame.len() + 19),
    ] {
        let mut tail = frame[..start].to_vec();
        tail.resize(end, 0);
        let mut bytes = log.bytes.clone();
        bytes.extend_from_slice(&tail);
        assert_row_full_agreement(&bytes);
        let (summary, classification) = fold::replay(bytes.as_slice()).unwrap();
        assert_eq!(summary.closed_end, offset as u64);
        assert!(matches!(classification,Tail::Torn{offset:at,..} if at==offset as u64));
        // A full-sized filled region followed by garbage still requires repair;
        // EOF-short bodies cannot distinguish such bytes from ordinary payload.
        bytes.push(1);
        assert_row_full_agreement(&bytes);
        if end < frame.len() {
            assert_eq!(
                fold::classify(&bytes).unwrap(),
                Tail::Torn {
                    offset: offset as u64,
                    kind: TornKind::ShortBody
                }
            );
        } else {
            assert_eq!(
                fault(fold::classify(&bytes).unwrap_err()),
                Fault::NonZeroAfterZeros {
                    offset: (offset + end) as u64
                }
            );
        }
    }
    // A normal all-zero empty-payload CRC cannot excuse structural corruption.
    frame[PREFIX_LEN] ^= 1;
    let mut bytes = log.bytes.clone();
    bytes.extend_from_slice(&frame);
    assert_row_full_agreement(&bytes);
    assert_eq!(
        fault(fold::classify(&bytes).unwrap_err()),
        Fault::Crc(Section::Structural)
    );
}
#[test]
fn zero_tail_garbage_reports_first_nonzero_absolute_offset() {
    let log = Log::first(false);
    for n in [1, 27, 28, 9000] {
        let mut bytes = log.bytes.clone();
        bytes.resize(bytes.len() + n, 0);
        bytes.extend_from_slice(&[4, 5, 0]);
        assert_row_full_agreement(&bytes);
        assert_eq!(
            fault(fold::classify(&bytes).unwrap_err()),
            Fault::NonZeroAfterZeros {
                offset: (log.bytes.len() + n) as u64
            }
        );
    }
}

#[test]
fn zeroed_structural_crc_is_torn_independent_of_last_data_byte() {
    let base = Log::first(false);
    for (seal_due, last) in [
        (None, 0xf6),
        (Some(SealDueReason::HardwareEarlyWarning), 0x00),
    ] {
        let mut c = commit(base.reference(), 2, 6, 1);
        c.seal_due = seal_due;
        let mut frame = Vec::new();
        encode_frame(&mut frame, base.next(), &Record::Committed(c), &[]).unwrap();
        let s = u32::from_le_bytes(frame[4..8].try_into().unwrap()) as usize;
        assert_eq!(frame[PREFIX_LEN + s - 1], last);
        frame[PREFIX_LEN + s..PREFIX_LEN + s + 8].fill(0);
        let mut bytes = base.bytes.clone();
        bytes.extend_from_slice(&frame);
        assert_row_full_agreement(&bytes);
        assert_eq!(
            fold::classify(&bytes).unwrap(),
            Tail::Torn {
                offset: base.bytes.len() as u64,
                kind: TornKind::ZeroFilled,
            }
        );
    }
}

#[test]
fn unclosed_group_retains_zero_fill_frame_offset() {
    let mut log = Log::default();
    log.push(init(false), &[]);
    let final_offset = log.bytes.len();
    log.push(Record::Committed(commit(log.reference(), 1, 2, 0)), &[]);
    let s = u32::from_le_bytes(
        log.bytes[final_offset + 4..final_offset + 8]
            .try_into()
            .unwrap(),
    ) as usize;
    log.bytes[final_offset + PREFIX_LEN + s..].fill(0);
    assert_row_full_agreement(&log.bytes);
    let (summary, tail) = fold::replay(log.bytes.as_slice()).unwrap();
    assert!(summary.is_fresh());
    assert_eq!(
        tail,
        Tail::Torn {
            offset: 0,
            kind: TornKind::UnclosedGroup {
                zero_filled_at: Some(final_offset as u64)
            },
        }
    );
}

#[test]
fn mid_log_zero_bit_flips_remain_crc_faults() {
    let mut first = Vec::new();
    encode_frame(
        &mut first,
        1,
        &Record::FinalizationRecoveryRequired { plan: 1 },
        &[],
    )
    .unwrap();
    assert_eq!(first[PREFIX_LEN], 1);
    for (index, section) in [(0, Section::Prefix), (PREFIX_LEN, Section::Structural)] {
        let mut bytes = first.clone();
        bytes[index] ^= 1;
        assert_eq!(bytes[index], 0);
        encode_frame(&mut bytes, 2, &init(false), &[]).unwrap();
        assert_row_full_agreement(&bytes);
        assert_eq!(
            fault(codec::classify_frames(&bytes).unwrap_err()),
            Fault::Crc(section)
        );
    }
}

// Compare the physical-tail layer used by replay with the row scanner. The fold
// additionally tests group closure/semantics, which row-only scans cannot prove.
fn assert_row_full_agreement(bytes: &[u8]) {
    let full = codec::classify_frames(bytes).map_err(fault);
    let row = (|| {
        let mut decoder = Decoder::new(Cursor::new(bytes));
        while decoder.next_row_frame()?.is_some() {}
        Ok(decoder.tail())
    })()
    .map_err(fault);
    assert_eq!(row, full);
}

#[test]
fn eof_short_body_with_internal_zero_bytes_is_torn() {
    let mut frame = Vec::new();
    encode_frame(&mut frame, 1, &init(false), &[]).unwrap();
    frame.truncate(PREFIX_LEN + 1); // One actual structural data byte.
    frame.extend_from_slice(&[0, 0, 0, 0x07]);
    assert_row_full_agreement(&frame);
    assert_eq!(
        codec::classify_frames(&frame).unwrap(),
        Tail::Torn {
            offset: 0,
            kind: TornKind::ShortBody
        }
    );
}

#[test]
fn row_and_full_classifier_agree_on_payload_zero_extension_and_damage() {
    let record = Record::PayloadContinuation(Continuation {
        group: 1,
        chunk: Chunk::Catalog {
            tape_file_number: 1,
            ordinal: 0,
            last: true,
        },
    });
    let mut frame = Vec::new();
    encode_frame(&mut frame, 1, &record, &[0x55; 40]).unwrap();
    let structural = u32::from_le_bytes(frame[4..8].try_into().unwrap()) as usize;
    let payload = PREFIX_LEN + structural + 8;
    for end in [payload + 5, frame.len(), frame.len() + 100] {
        let mut bytes = frame[..payload + 2].to_vec();
        bytes.resize(end, 0);
        assert_row_full_agreement(&bytes);
        assert_eq!(
            codec::classify_frames(&bytes).unwrap(),
            Tail::Torn {
                offset: 0,
                kind: TornKind::ZeroFilled,
            }
        );
        bytes.push(7);
        assert_row_full_agreement(&bytes);
        if end < frame.len() {
            assert_eq!(
                codec::classify_frames(&bytes).unwrap(),
                Tail::Torn {
                    offset: 0,
                    kind: TornKind::ShortBody
                }
            );
        } else {
            let expected = Fault::NonZeroAfterZeros { offset: end as u64 };
            assert_eq!(fault(codec::classify_frames(&bytes).unwrap_err()), expected);
        }
    }
}

#[path = "store_tests.rs"]
mod disk;

#[test]
fn parity_off_total_grows_and_refuses_regression_and_protection() {
    let first = Log::first(false);
    assert_eq!((first.summary().protected, first.summary().total), (0, 3));
    assert!(first.summary().uncovered.is_empty());
    for (total, protected, valid) in [(6, 0, true), (2, 0, false), (6, 1, false)] {
        let mut log = Log::first(false);
        let row = object(2, 6, None, 3);
        log.row(row.clone());
        log.catalog(&row, 6, 0, 31);
        let mut c = commit(log.reference(), 3, 10, 2);
        c.total = total;
        c.protected = protected;
        log.push(Record::Committed(c), &[]);
        let result = fold::replay(log.bytes.as_slice());
        assert_eq!(result.is_ok(), valid);
        if let Ok((summary, _)) = result {
            assert_eq!(summary.total, 6);
            assert!(summary.uncovered.is_empty());
        }
    }
}

// Exercise both readers at every cut after a contradiction first becomes visible.
fn short_prefix_fault(mut prefix: Vec<u8>, first_visible: usize, expected: Fault) {
    let base = Log::first(false);
    prefix.truncate(PREFIX_LEN);
    for end in first_visible..PREFIX_LEN {
        let mut bytes = base.bytes.clone();
        bytes.extend_from_slice(&prefix[..end]);
        assert_row_full_agreement(&bytes);
        assert_eq!(
            fault(fold::replay(bytes.as_slice()).unwrap_err()),
            expected,
            "cut={end}"
        );
    }
}
fn valid_next_prefix() -> Vec<u8> {
    let base = Log::first(false);
    let mut bytes = Vec::new();
    encode_frame(
        &mut bytes,
        base.next(),
        &Record::Committed(commit(base.reference(), 2, 6, 1)),
        &[],
    )
    .unwrap();
    bytes
}
#[test]
fn short_prefix_refuses_visible_version_contradictions() {
    let mut bytes = valid_next_prefix();
    bytes[0] = 2;
    short_prefix_fault(bytes, 1, Fault::Version(2));
    let mut bytes = valid_next_prefix();
    bytes[1] = 1;
    short_prefix_fault(bytes, 2, Fault::Version(257));
}
#[test]
fn short_prefix_refuses_visible_unknown_and_reserved_tags() {
    for (value, expected) in [
        (11u16, Fault::UnknownTag(11)),
        (16, Fault::ReservedTag(Tag::FinalizationPlanned)),
    ] {
        let mut bytes = valid_next_prefix();
        bytes[2..4].copy_from_slice(&value.to_le_bytes());
        short_prefix_fault(bytes, 3, expected);
    }
    let mut bytes = valid_next_prefix();
    bytes[3] = 1;
    short_prefix_fault(bytes, 4, Fault::UnknownTag(259));
}
#[test]
fn short_prefix_refuses_visible_body_length_overflow() {
    let mut bytes = valid_next_prefix();
    bytes[4..8].copy_from_slice(&(MAX_FRAME_BODY as u32 + 1).to_le_bytes());
    short_prefix_fault(bytes, 8, Fault::FrameTooLarge);
    let mut bytes = valid_next_prefix();
    bytes[4..8].copy_from_slice(&(MAX_FRAME_BODY as u32).to_le_bytes());
    bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
    short_prefix_fault(bytes, 9, Fault::FrameTooLarge);
}
#[test]
fn short_prefix_refuses_visible_sequence_mismatch() {
    let base = Log::first(false);
    let mut bytes = valid_next_prefix();
    bytes[12..20].copy_from_slice(&(base.next() + 1).to_le_bytes());
    short_prefix_fault(
        bytes,
        13,
        Fault::Sequence {
            expected: base.next(),
            actual: base.next() + 1,
        },
    );
    let mut bytes = valid_next_prefix();
    bytes[19] = 1;
    short_prefix_fault(
        bytes,
        20,
        Fault::Sequence {
            expected: base.next(),
            actual: base.next() + (1 << 56),
        },
    );
}
#[test]
fn short_prefix_accepts_compatible_partial_sequence_bytes() {
    let expected = 0x1122334455667788;
    let mut bytes = valid_next_prefix();
    bytes[12..20].copy_from_slice(&u64::to_le_bytes(expected));
    for cut in 13..PREFIX_LEN {
        let mut decoder = Decoder::at(&bytes[..cut], expected, 0);
        assert!(decoder.next_frame().unwrap().is_none());
        assert_eq!(
            decoder.tail(),
            Tail::Torn {
                offset: 0,
                kind: TornKind::ShortPrefix
            }
        );
    }
}
