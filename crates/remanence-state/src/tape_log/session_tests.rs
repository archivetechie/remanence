//! V2 causal transitions, streaming tail integrity and original range ages.
use super::*;

fn opened(log: &Log) -> SessionOpened {
    let s = log.summary();
    SessionOpened {
        session_id: [9; 16],
        incarnation: 1,
        base_media_sequence: s.last_media_commit.unwrap(),
        cursor: SessionCursor {
            partition: s.cursor.partition,
            lba: s.cursor.lba,
            next_file: s.next_file,
        },
        opened_at_utc: "2026-11-02T00:00:00Z".into(),
    }
}
fn session(parity: bool) -> Log {
    let mut log = Log::default();
    log.push(init(parity), &[]);
    log.push(Record::SessionOpened(opened(&log)), &[]);
    log
}
fn ended(log: &Log, count: u64) -> SessionEnded {
    let o = opened(log);
    SessionEnded {
        session_id: o.session_id,
        incarnation: 1,
        base_media_sequence: o.base_media_sequence,
        cursor: o.cursor,
        ended_at_utc: "2026-11-02T00:01:00Z".into(),
        reason: SessionEndReason::Abort,
        tail: log.reference(),
        tail_file_count: count,
        crash_ack_audit_sequence: None,
    }
}
fn tail(log: &mut Log, file: u64, known: u64, unknown: u64, mark: TailFilemark) {
    log.push(
        Record::PayloadContinuation(Continuation {
            group: log.group_start(),
            chunk: Chunk::TailFile(TailFile {
                tape_file_number: file,
                identity: TailIdentity::Object("object".into()),
                known_blocks: known,
                unknown_blocks: unknown,
                filemark: mark,
            }),
        }),
        &[],
    );
}
fn debt() -> Log {
    let mut log = session(true);
    let row = object(1, 2, Some(0), 3);
    log.row(row.clone());
    log.catalog(&row, 3, 0, 17);
    let mut c = commit(log.reference(), 2, 6, 1);
    c.check = CheckValue(42);
    c.provenance = CheckProvenance::OriginalWrite;
    log.push(Record::Committed(c), &[]);
    log
}
fn close(log: &Log) -> ClosedOut {
    ClosedOut {
        group: log.reference(),
        incarnation: 1,
        operation_id: [3; 16],
        session_id: Some([9; 16]),
        base_media_sequence: log.summary().last_media_commit.unwrap(),
        from_w: 0,
        to_t: 3,
        epoch_id: 0,
        sidecar_file_number: 2,
        check_before: CheckValue(42),
        cursor_after: CommitCursor {
            partition: 0,
            lba: 14,
        },
        next_file: 3,
        next_epoch: 1,
        completed_at_utc: "2026-11-02T00:02:00Z".into(),
    }
}
#[test]
fn session_records_roundtrip_and_metadata_preserves_media() {
    for reason in [
        SessionEndReason::CleanClose,
        SessionEndReason::Abort,
        SessionEndReason::FailedAppend,
        SessionEndReason::ClientStall,
        SessionEndReason::DriveError,
        SessionEndReason::GracefulShutdown,
        SessionEndReason::ClientDisconnected,
        SessionEndReason::CrashRecovered,
        SessionEndReason::CloseoutFailed,
    ] {
        let mut log = session(false);
        let prior = log.summary();
        let mut end = ended(&log, 0);
        end.reason = reason;
        for record in [
            Record::SessionOpened(opened(&log)),
            Record::SessionEnded(end.clone()),
        ] {
            let mut bytes = Vec::new();
            wire::encode(&record, &mut bytes).unwrap();
            assert_eq!(wire::decode(record.tag(), &bytes).unwrap(), record);
        }
        log.push(Record::SessionEnded(end), &[]);
        let s = log.summary();
        assert_eq!(
            (
                s.cursor,
                s.next_file,
                s.protected,
                s.total,
                s.barrier_count,
                s.object_count,
                s.last_media_commit
            ),
            (
                prior.cursor,
                prior.next_file,
                prior.protected,
                prior.total,
                prior.barrier_count,
                prior.object_count,
                prior.last_media_commit
            )
        );
        assert!(s.open_session.is_none());
        log.push(Record::SessionOpened(opened(&log)), &[]);
        assert!(log.summary().last_session_end.is_none());
    }
}
#[test]
fn session_illegal_identity_sequence_cursor_and_double_open_refuse() {
    for mutation in 0..8 {
        let mut log = session(false);
        let mut end = ended(&log, 0);
        match mutation {
            0 => end.session_id = [8; 16],
            1 => end.incarnation = 2,
            2 => end.base_media_sequence += 1,
            3 => end.cursor.partition = 1,
            4 => end.cursor.lba += 1,
            5 => end.cursor.next_file += 1,
            6 => end.ended_at_utc = "not a timestamp".into(),
            _ => end.tail_file_count = 1,
        }
        log.push(Record::SessionEnded(end), &[]);
        assert!(fold::replay(&log.bytes[..]).is_err(), "mutation {mutation}");
    }
    let mut log = session(false);
    log.push(Record::SessionOpened(opened(&log)), &[]);
    assert!(fold::replay(&log.bytes[..]).is_err());
    let mut log = session(false);
    let end = ended(&log, 0);
    log.push(Record::SessionEnded(end.clone()), &[]);
    let mut end = end;
    end.tail = log.reference();
    log.push(Record::SessionEnded(end), &[]);
    assert!(fold::replay(&log.bytes[..]).is_err());
}
#[test]
fn tail_cuts_at_every_byte_never_publish_a_partial_end() {
    let mut log = session(false);
    let prefix = log.bytes.len();
    let before = log.summary();
    tail(&mut log, 1, 4, 0, TailFilemark::Present);
    log.push(
        Record::PayloadContinuation(Continuation {
            group: log.group_start(),
            chunk: Chunk::TailFile(TailFile {
                tape_file_number: 2,
                identity: TailIdentity::Sidecar([4; 32]),
                known_blocks: 0,
                unknown_blocks: 3,
                filemark: TailFilemark::Absent,
            }),
        }),
        &[],
    );
    let end = ended(&log, 2);
    log.push(Record::SessionEnded(end), &[]);
    for cut in prefix..log.bytes.len() {
        let (s, _) = fold::replay(&log.bytes[..cut]).unwrap();
        assert!(s.open_session.is_some(), "cut {cut}");
        assert!(s.last_session_end.is_none());
        assert_eq!(s.last_closed_sequence, before.last_closed_sequence);
    }
    assert_eq!(log.summary().last_session_end.unwrap().tail_file_count, 2);
}
#[test]
fn tail_rejects_gaps_overflow_mixed_rows_and_nonfinal_uncertainty() {
    for mutation in 0..6 {
        let mut log = session(false);
        match mutation {
            0 => tail(&mut log, 2, 1, 0, TailFilemark::Present),
            1 => tail(&mut log, 1, u64::MAX, 1, TailFilemark::Unknown),
            2 => tail(&mut log, 1, 0, 1, TailFilemark::Present),
            3 => {
                tail(&mut log, 1, 1, 1, TailFilemark::Unknown);
                tail(&mut log, 2, 1, 0, TailFilemark::Present);
            }
            4 => {
                tail(&mut log, 1, 1, 0, TailFilemark::Present);
                log.row(object(2, 4, None, 3));
            }
            _ => tail(&mut log, 1, 0, 0, TailFilemark::Present),
        }
        // Construct the end without trying to replay the deliberately corrupt prefix.
        let mut end = ended(&session(false), 1);
        end.tail = log.reference();
        log.push(Record::SessionEnded(end), &[]);
        assert!(fold::replay(&log.bytes[..]).is_err(), "mutation {mutation}");
    }
    let mut log = session(false);
    tail(&mut log, 1, 0, 1, TailFilemark::Absent);
    let end = ended(&log, 1);
    log.push(Record::SessionEnded(end), &[]);
    assert!(log.summary().open_session.is_none());
}
#[test]
fn closeout_proves_exact_range_check_epoch_extent_and_clears_age() {
    for mutation in 0..14 {
        let mut log = debt();
        let mut row = sidecar(2, 6, 0, 0, 3);
        row.block_count = if mutation == 13 { 8 } else { 7 };
        log.row(row);
        let mut c = close(&log);
        match mutation {
            0 => {}
            1 => c.from_w = 1,
            2 => c.to_t = 2,
            3 => c.check_before = CheckValue(41),
            4 => c.epoch_id = 1,
            5 => c.sidecar_file_number = 3,
            6 => c.cursor_after.lba += 1,
            7 => c.cursor_after.partition = 1,
            8 => c.next_file += 1,
            9 => c.next_epoch += 1,
            10 => c.session_id = Some([8; 16]),
            11 => c.base_media_sequence += 1,
            12 => c.session_id = None,
            _ => c.cursor_after.lba += 1,
        }
        let mut encoded = Vec::new();
        wire::encode(&Record::ClosedOut(c.clone()), &mut encoded).unwrap();
        assert_eq!(
            wire::decode(Tag::ClosedOut, &encoded).unwrap(),
            Record::ClosedOut(c.clone())
        );
        log.push(Record::ClosedOut(c), &[]);
        if mutation == 0 {
            let s = log.summary();
            assert_eq!((s.protected, s.total), (3, 3));
            assert!(s.uncovered.is_empty());
            assert_eq!(s.check, CheckValue::EMPTY);
            assert_eq!(s.unprotected_since, None);
            assert_eq!(s.cursor.lba, 14);
        } else {
            assert!(fold::replay(&log.bytes[..]).is_err(), "mutation {mutation}");
        }
    }
}
#[test]
fn empty_commits_and_session_metadata_preserve_original_age() {
    let mut log = debt();
    let age = log.summary().unprotected_since;
    let mut c = commit(log.reference(), 2, 6, 1);
    c.check = CheckValue(42);
    c.provenance = CheckProvenance::OriginalWrite;
    c.committed_at_utc = "2026-11-02T02:00:00Z".into();
    log.push(Record::Committed(c), &[]);
    assert_eq!(log.summary().unprotected_since, age);
    let end = ended(&log, 0);
    log.push(Record::SessionEnded(end), &[]);
    assert_eq!(log.summary().unprotected_since, age);
}
#[test]
fn v2_maps_reject_duplicate_keys_wrong_types_nonminimal_and_overflow() {
    let log = session(false);
    let record = Record::SessionOpened(opened(&log));
    let mut bytes = Vec::new();
    wire::encode(&record, &mut bytes).unwrap();
    // Map(5), key0, bytes(16), session; key1 (incarnation) at offset20.
    assert_eq!(bytes[20], 1);
    for replacement in [
        vec![0, 1],
        vec![1, 0xf5],
        vec![1, 0x18, 1],
        vec![1, 0x3b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
    ] {
        let mut corrupt = bytes.clone();
        corrupt.splice(20..22, replacement);
        assert!(wire::decode(Tag::SessionOpened, &corrupt).is_err());
    }
    let mut cursor = Vec::new();
    SessionCursor {
        partition: 0,
        lba: 0,
        next_file: 0,
    }
    .put(&mut cursor)
    .unwrap();
    cursor.splice(2..3, [0x1b, 0, 0, 0, 1, 0, 0, 0, 0]);
    assert!(SessionCursor::get(&mut &cursor[..]).is_err());
}
#[test]
fn large_named_tail_folds_with_constant_retained_state() {
    let log = session(false);
    let mut decoder = Decoder::new(&log.bytes[..]);
    let mut fold = Fold::new();
    while let Some(frame) = decoder.next_frame().unwrap() {
        fold.push(&frame).unwrap();
    }
    let base = log.next();
    let mut offset = log.bytes.len() as u64;
    let mut digest = GroupDigest::new(base, offset);
    const FILES: u64 = 100_000;
    for n in 0..FILES {
        let record = Record::PayloadContinuation(Continuation {
            group: base,
            chunk: Chunk::TailFile(TailFile {
                tape_file_number: n + 1,
                identity: TailIdentity::Object("object".into()),
                known_blocks: 1,
                unknown_blocks: 0,
                filemark: TailFilemark::Present,
            }),
        });
        let mut bytes = Vec::new();
        encode_frame(&mut bytes, base + n, &record, &[]).unwrap();
        let mut decoder = Decoder::at(&bytes[..], base + n, offset);
        let frame = decoder.next_frame().unwrap().unwrap();
        digest.push(&frame).unwrap();
        fold.push(&frame).unwrap();
        offset = frame.end;
        assert_eq!(fold.summary().last_closed_sequence, base - 1);
        assert!(fold.summary().uncovered.is_empty());
    }
    let mut end = ended(&log, FILES);
    end.tail = digest.reference(0);
    let mut bytes = Vec::new();
    encode_frame(&mut bytes, base + FILES, &Record::SessionEnded(end), &[]).unwrap();
    let mut decoder = Decoder::at(&bytes[..], base + FILES, offset);
    fold.push(&decoder.next_frame().unwrap().unwrap()).unwrap();
    assert_eq!(
        fold.summary()
            .last_session_end
            .as_ref()
            .unwrap()
            .tail_file_count,
        FILES
    );
}

#[test]
fn object_commits_require_session_identity_and_typed_triggers_roundtrip() {
    for id in [None, Some([0; 16]), Some([8; 16])] {
        let mut log = session(false);
        let row = object(1, 2, None, 3);
        log.row(row.clone());
        log.catalog(&row, 3, 0, 17);
        let mut c = commit(log.reference(), 2, 6, 1);
        c.session_id = id;
        log.push(Record::Committed(c), &[]);
        assert!(fold::replay(&log.bytes[..]).is_err());
    }
    for trigger in [
        TerminalFinalizationTrigger::ReachedLowWatermark,
        TerminalFinalizationTrigger::HardwareEarlyWarning,
        TerminalFinalizationTrigger::OperatorCloseOut,
        TerminalFinalizationTrigger::PoolCloseOut,
        TerminalFinalizationTrigger::NoPendingObjectFits,
    ] {
        let mut log = session(false);
        let row = object(1, 2, None, 3);
        log.row(row.clone());
        log.catalog(&row, 3, 0, 17);
        let mut c = commit(log.reference(), 2, 6, 1);
        c.seal_due = Some(trigger);
        let record = Record::Committed(c);
        let mut bytes = Vec::new();
        wire::encode(&record, &mut bytes).unwrap();
        assert_eq!(wire::decode(Tag::Committed, &bytes).unwrap(), record);
        log.push(record, &[]);
        assert_eq!(log.summary().seal_due, Some(trigger));
    }
}
