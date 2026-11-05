//! Contract tests for normalized completion evidence and typed device policy.
use super::*;
use crate::ScsiError;

/// Synthetic count conflicts exercise the one rule before any consumer refines
/// the evidence: valid current sense wins, absent/deferred sense falls back.
#[test]
fn b4b2c_classifier_residual_precedence_and_fallback() {
    for class in [CommandClass::Read, CommandClass::Write] {
        for block_size in [None, Some(512)] {
            for host in [0, 128, 1024, 2048] {
                for (response, residual, expected) in [
                    (0xf0, 0, 1024),
                    (0xf0, if block_size.is_some() { 1 } else { 512 }, 512),
                    (0xf0, if block_size.is_some() { 2 } else { 1024 }, 0),
                    (0x70, 1, host),
                    (0xf1, 1, host),
                ] {
                    let mut raw = sense(8, 0, 5);
                    raw[0] = response;
                    raw[3..7].copy_from_slice(&u32::to_be_bytes(residual));
                    let mut evidence = CompletionEvidence::from_scsi(&ScsiError::CheckCondition {
                        sense: raw,
                        bytes_transferred: host,
                    });
                    evidence.requested_bytes = Some(1024);
                    evidence.block_size = block_size;
                    let outcome = classify(input(
                        class,
                        Phase::Read,
                        Region::AtOrAfterCursor(10),
                        evidence,
                    ));
                    assert_eq!(
                        outcome.evidence.transferred_bytes,
                        Some(u64::from(expected))
                    );
                    assert_eq!(
                        outcome.evidence.host_transferred_bytes,
                        Some(u64::from(host))
                    );
                    assert_eq!(
                        outcome.evidence.boundary.physical_eod,
                        response != 0xf1 && expected == 0
                    );
                }
            }
        }
    }
}

fn sense(key: u8, asc: u8, ascq: u8) -> Vec<u8> {
    let mut sense = vec![0; 18];
    sense[0] = 0x70;
    sense[2] = key;
    sense[7] = 10;
    sense[12] = asc;
    sense[13] = ascq;
    sense
}
fn check(key: u8, asc: u8, ascq: u8) -> CompletionEvidence {
    CompletionEvidence::from_scsi(&ScsiError::CheckCondition {
        sense: sense(key, asc, ascq),
        bytes_transferred: 0,
    })
}
fn input(
    class: CommandClass,
    phase: Phase,
    region: Region,
    evidence: CompletionEvidence,
) -> OutcomeInput {
    OutcomeInput {
        class,
        context: OperationContext {
            phase,
            region,
            ..OperationContext::default()
        },
        evidence,
    }
}

/// Raw sense is retained, but truncated sense cannot supply any boundary or medium fact.
#[test]
fn b3b_unusable_sense_has_no_policy_facts() {
    for key in 0..16 {
        let bytes = vec![0x70, 0, key | 0xe0];
        let evidence = CompletionEvidence::from_scsi(&ScsiError::CheckCondition {
            sense: bytes.clone(),
            bytes_transferred: 0,
        });
        assert_eq!(evidence.completion, Completion::Unknown);
        assert_eq!(evidence.sense, Some(bytes));
        assert_eq!(evidence.decoded, None);
        assert_eq!(evidence.boundary, BoundaryFlags::default());
    }
}

/// A host failure retains all captured fields and never establishes medium damage.
#[test]
fn b3b_host_failure_dominates_usable_sense() {
    let e = CompletionEvidence::from_scsi(&ScsiError::TransportError {
        status: 2,
        host_status: 3,
        driver_status: 8,
        info: 1,
        sense: sense(3, 0x11, 0),
    });
    assert_eq!(e.host_status, Some(3));
    assert_eq!(e.driver_status, Some(8));
    assert_eq!(e.scsi_status, Some(2));
    assert!(!e.current_medium_damage());
    let out = classify(input(
        CommandClass::Read,
        Phase::Read,
        Region::Committed(10..14),
        e,
    ));
    assert_eq!(out.effect, Effect::Transient);
    assert_eq!(out.retry, RetryCause::Transient);
}

/// Typed transport failures cannot acquire a different fence cause from diagnostic words.
#[test]
fn b3b_fence_reason_ignores_message_text() {
    for message in [
        "position drift",
        "partial fixed batch uncommittable",
        "append admission",
        "timeout",
    ] {
        let e = CompletionEvidence::from_scsi(&ScsiError::Io(std::io::Error::other(message)));
        let out = classify(input(
            CommandClass::Write,
            Phase::Append,
            Region::AtOrAfterCursor(10),
            e,
        ));
        assert_eq!(out.effect, Effect::WriteFence(FenceCause::TransferError));
        assert_eq!(out.retry, RetryCause::Transient);
        assert!(out.requires_reproof);
    }
}

/// Definite zero-transfer rejection is distinct from an unproved WRITE acceptance.
#[test]
fn b3b_definite_rejection_and_medium_write() {
    for key in [5, 7] {
        let out = classify(input(
            CommandClass::Write,
            Phase::Append,
            Region::AtOrAfterCursor(10),
            check(key, 0x24, 0),
        ));
        assert_eq!(out.effect, Effect::NoFence);
        assert_eq!(out.retry, RetryCause::Transient);
    }
    let out = classify(input(
        CommandClass::Write,
        Phase::Append,
        Region::AtOrAfterCursor(10),
        check(3, 0x0c, 0),
    ));
    assert_eq!(out.effect, Effect::WriteFence(FenceCause::TransferError));
    assert_eq!(out.retry, RetryCause::Media);
}

/// Committed damage is a read finding; a proof of the append cursor uses the contradiction policy.
#[test]
fn b3b_read_damage_and_write_contradiction() {
    for phase in [Phase::Read, Phase::Verification] {
        let out = classify(input(
            CommandClass::Read,
            phase,
            Region::Committed(10..14),
            check(3, 0x11, 0),
        ));
        assert_eq!(out.effect, Effect::ReadFinding(10..14));
    }
    for phase in [Phase::Admission, Phase::Closeout, Phase::Finalization] {
        let out = classify(input(
            CommandClass::Read,
            phase,
            Region::Committed(10..14),
            check(3, 0x11, 0),
        ));
        assert!(matches!(out.effect, Effect::WriteFence(_)));
    }
}

/// Only publication durability changes an ambiguous append into projection unavailability.
#[test]
fn b3b_authority_durability_changes_effect() {
    let mut i = input(
        CommandClass::OtherControl,
        Phase::LogAppend,
        Region::AtOrAfterCursor(10),
        CompletionEvidence::success(),
    );
    i.context.durability.authority_append = AuthorityAppend::Ambiguous;
    assert_eq!(
        classify(i.clone()).effect,
        Effect::WriteFence(FenceCause::AuthorityAmbiguous)
    );
    i.context.durability.authority_append = AuthorityAppend::Durable;
    i.context.durability.projection = Projection::Pending;
    let out = classify(i);
    assert_eq!(out.effect, Effect::NoFence);
    assert_eq!(out.reason, OutcomeReason::ProjectionUnavailable);
}

/// Only durable tail proof exempts an unknown write, including mounted writes.
#[test]
fn b3b_named_tail_and_mounted_exception() {
    let mut i = input(
        CommandClass::Write,
        Phase::Append,
        Region::AtOrAfterCursor(10),
        CompletionEvidence::unknown(),
    );
    i.context.durability.named_live_tail = true;
    assert_eq!(classify(i.clone()).effect, Effect::NoFence);
    i.context.mounted = true;
    let named = classify(i.clone());
    assert_eq!(named.effect, Effect::NoFence);
    assert_eq!(named.reason, OutcomeReason::NamedLiveTail);
    i.context.durability.named_live_tail = false;
    assert_eq!(
        classify(i).effect,
        Effect::WriteFence(FenceCause::TransferError)
    );
}

/// Unknown motion is scoped to the supplied partition and retains transient retry.
#[test]
fn b3b_motion_partition_and_known_rejections() {
    for class in [CommandClass::Move, CommandClass::Load, CommandClass::Unload] {
        let mut i = input(
            class,
            Phase::Motion,
            Region::NotTape,
            CompletionEvidence::unknown(),
        );
        i.context.partition = PartitionIdentity::Standalone(b"drive-a".to_vec());
        assert_eq!(
            classify(i).effect,
            Effect::InventoryUnknown(PartitionIdentity::Standalone(b"drive-a".to_vec()))
        );
    }
    for ascq in [0x0d, 0x0e] {
        let out = classify(input(
            CommandClass::Move,
            Phase::Motion,
            Region::NotTape,
            check(5, 0x3b, ascq),
        ));
        assert_eq!(out.effect, Effect::NoFence);
    }
}

/// Absence of residual evidence is not a valid zero-residual claim.
#[test]
fn b3b_residual_absence_and_zero_are_distinct() {
    let none = check(3, 0x0c, 0);
    let mut raw = sense(3, 0x0c, 0);
    raw[0] |= 0x80;
    let zero = CompletionEvidence::from_scsi(&ScsiError::CheckCondition {
        sense: raw,
        bytes_transferred: 0,
    });
    assert_eq!(none.residual, None);
    assert_eq!(zero.residual, Some(0));
}

/// Secondary position errors cannot erase the failed WRITE's validated partial counts.
#[test]
fn b3b_wrapped_partial_write_keeps_primary_evidence() {
    let error = crate::TapeIoError::new(crate::TapeIoErrorKind::WriteFailureWithPositionError {
        write_error: Box::new(crate::TapeIoError::new(
            crate::TapeIoErrorKind::PartialBatchUncommittable {
                requested_records: 4,
                written_records: 2,
                requested_bytes: 2048,
                written_bytes: 1024,
                end_of_medium: false,
                sense: Some(sense(3, 0x0c, 0)),
            },
        )),
        position_error: Box::new(crate::TapeIoError::new(crate::TapeIoErrorKind::Transport(
            ScsiError::Io(std::io::Error::other("position drift")),
        ))),
    });
    let e = error.completion_evidence();
    assert_eq!(e.records_transferred, Some(2));
    assert_eq!(
        classify(input(
            CommandClass::Write,
            Phase::Append,
            Region::AtOrAfterCursor(10),
            e
        ))
        .effect,
        Effect::WriteFence(FenceCause::PartialBatch)
    );
}

/// Partial batches preserve the base retry rule: usable medium/hardware sense only.
#[test]
fn b3b_partial_write_retry_requires_usable_medium_evidence() {
    for (sense, retry) in [
        (None, RetryCause::Transient),
        (Some(vec![0x70, 0, 3]), RetryCause::Transient),
        (Some(sense(3, 0x0c, 0)), RetryCause::Media),
        (Some(sense(0, 0, 2)), RetryCause::Transient),
    ] {
        let error = crate::TapeIoError::new(crate::TapeIoErrorKind::PartialBatchUncommittable {
            requested_records: 4,
            written_records: 2,
            requested_bytes: 2048,
            written_bytes: 1024,
            end_of_medium: false,
            sense,
        });
        for class in [CommandClass::Write, CommandClass::OtherControl] {
            let outcome = classify(input(
                class,
                Phase::Append,
                Region::AtOrAfterCursor(10),
                error.completion_evidence(),
            ));
            assert_eq!(outcome.retry, retry);
        }
    }
}

/// A READ POSITION failure in a read scope does not establish damage to data records.
#[test]
fn b3b_position_failure_does_not_claim_read_damage() {
    for phase in [
        Phase::Read,
        Phase::Verification,
        Phase::Admission,
        Phase::Closeout,
    ] {
        let outcome = classify(input(
            CommandClass::Position,
            phase,
            Region::Committed(10..11),
            check(3, 0x11, 0),
        ));
        assert_eq!(outcome.effect, Effect::Transient, "{phase:?}");
        assert_eq!(outcome.retry, RetryCause::Media);
    }
}

/// An owner's separately proved read contradiction retains its original fence.
#[test]
fn b3b_composite_read_contradiction_preserves_fence() {
    for (phase, cause) in [
        (Phase::Admission, FenceCause::SessionSuffixContradiction),
        (Phase::Closeout, FenceCause::CloseoutReadContradiction),
    ] {
        let mut evidence = CompletionEvidence::success();
        evidence.failed = true;
        evidence.fact = CompletionFact::CursorContradiction;
        let outcome = classify(input(
            CommandClass::OtherControl,
            phase,
            Region::Committed(10..11),
            evidence,
        ));
        assert_eq!(outcome.effect, Effect::WriteFence(cause));
    }
}

/// Every historical W2 case has an explicit vector; coverage names are checked against the frozen trace.
#[test]
fn b3b_w2_trace_cases() {
    // Each row names its own input and expectation. The TSV only checks coverage,
    // so editing its expected_effect column cannot silently rewrite this oracle.
    type Vector = (
        &'static str,
        CommandClass,
        Phase,
        Region,
        u8,
        u8,
        u8,
        u8,
        u8,
    );
    let vectors: &[Vector] = &[
        (
            "replay_failure",
            CommandClass::OtherControl,
            Phase::Closeout,
            Region::Committed(10..14),
            6,
            0,
            0,
            0,
            0,
        ),
        (
            "journal_corrupt",
            CommandClass::OtherControl,
            Phase::Closeout,
            Region::Committed(10..14),
            4,
            0,
            0,
            0,
            1,
        ),
        (
            "open_tail_catalog_read",
            CommandClass::OtherControl,
            Phase::Admission,
            Region::NotTape,
            3,
            0,
            0,
            0,
            0,
        ),
        (
            "suffix_medium",
            CommandClass::Read,
            Phase::Closeout,
            Region::AtOrAfterCursor(14),
            1,
            3,
            17,
            0,
            2,
        ),
        (
            "suffix_scan_medium",
            CommandClass::Read,
            Phase::Closeout,
            Region::AtOrAfterCursor(14),
            1,
            3,
            17,
            0,
            2,
        ),
        (
            "open_admission_blank",
            CommandClass::Position,
            Phase::Admission,
            Region::AtOrAfterCursor(14),
            1,
            8,
            0,
            0,
            1,
        ),
        (
            "open_admission_medium",
            CommandClass::Position,
            Phase::Admission,
            Region::AtOrAfterCursor(14),
            1,
            3,
            17,
            0,
            1,
        ),
        (
            "open_admission_unit_attention",
            CommandClass::Position,
            Phase::Admission,
            Region::AtOrAfterCursor(14),
            1,
            6,
            41,
            0,
            2,
        ),
        (
            "open_admission_not_ready",
            CommandClass::Position,
            Phase::Admission,
            Region::AtOrAfterCursor(14),
            1,
            2,
            4,
            0,
            2,
        ),
        (
            "open_admission_timeout",
            CommandClass::Position,
            Phase::Admission,
            Region::AtOrAfterCursor(14),
            2,
            0,
            0,
            0,
            2,
        ),
        (
            "unit_attention",
            CommandClass::Read,
            Phase::Closeout,
            Region::Committed(10..14),
            1,
            6,
            41,
            0,
            2,
        ),
        (
            "timeout",
            CommandClass::Read,
            Phase::Closeout,
            Region::Committed(10..14),
            2,
            0,
            0,
            0,
            2,
        ),
        (
            "not_ready",
            CommandClass::Read,
            Phase::Closeout,
            Region::Committed(10..14),
            1,
            2,
            4,
            0,
            2,
        ),
        (
            "transport",
            CommandClass::Read,
            Phase::Closeout,
            Region::Committed(10..14),
            2,
            0,
            0,
            0,
            2,
        ),
        (
            "position_read",
            CommandClass::Position,
            Phase::Closeout,
            Region::Committed(10..14),
            2,
            0,
            0,
            0,
            2,
        ),
        (
            "wrong_length",
            CommandClass::Read,
            Phase::Closeout,
            Region::Committed(10..14),
            1,
            0,
            0,
            32,
            1,
        ),
        (
            "hardware_44",
            CommandClass::Read,
            Phase::Closeout,
            Region::Committed(10..14),
            1,
            4,
            68,
            0,
            2,
        ),
        (
            "hardware_15",
            CommandClass::Read,
            Phase::Closeout,
            Region::Committed(10..14),
            1,
            4,
            21,
            0,
            2,
        ),
        (
            "bot_filemark",
            CommandClass::Read,
            Phase::Admission,
            Region::Bot,
            1,
            0,
            0,
            128,
            1,
        ),
        (
            "bot_blank",
            CommandClass::Read,
            Phase::Admission,
            Region::Bot,
            1,
            8,
            0,
            0,
            1,
        ),
        (
            "bot_medium",
            CommandClass::Read,
            Phase::Admission,
            Region::Bot,
            1,
            3,
            17,
            0,
            1,
        ),
        (
            "bot_oversized",
            CommandClass::Read,
            Phase::Admission,
            Region::Bot,
            1,
            0,
            0,
            32,
            1,
        ),
        (
            "bot_transport",
            CommandClass::Read,
            Phase::Admission,
            Region::Bot,
            2,
            0,
            0,
            0,
            2,
        ),
        (
            "admission_filemark",
            CommandClass::Position,
            Phase::Admission,
            Region::AtOrAfterCursor(14),
            1,
            0,
            0,
            128,
            1,
        ),
        (
            "admission_medium",
            CommandClass::Position,
            Phase::Admission,
            Region::AtOrAfterCursor(14),
            1,
            3,
            17,
            0,
            1,
        ),
        (
            "admission_transport",
            CommandClass::Position,
            Phase::Admission,
            Region::AtOrAfterCursor(14),
            2,
            0,
            0,
            0,
            2,
        ),
        (
            "admission_unit_attention",
            CommandClass::Position,
            Phase::Admission,
            Region::AtOrAfterCursor(14),
            1,
            6,
            41,
            0,
            2,
        ),
        (
            "admission_not_ready",
            CommandClass::Position,
            Phase::Admission,
            Region::AtOrAfterCursor(14),
            1,
            2,
            4,
            0,
            2,
        ),
        (
            "open_unit_attention",
            CommandClass::Read,
            Phase::Admission,
            Region::Committed(10..14),
            1,
            6,
            41,
            0,
            2,
        ),
        (
            "open_timeout",
            CommandClass::Read,
            Phase::Admission,
            Region::Committed(10..14),
            2,
            0,
            0,
            0,
            2,
        ),
        (
            "filemark",
            CommandClass::Read,
            Phase::Closeout,
            Region::Committed(10..14),
            1,
            0,
            0,
            128,
            1,
        ),
        (
            "eod",
            CommandClass::Read,
            Phase::Closeout,
            Region::Committed(10..14),
            1,
            8,
            0,
            0,
            1,
        ),
        (
            "blank_locate",
            CommandClass::Position,
            Phase::Closeout,
            Region::AtOrAfterCursor(14),
            1,
            8,
            0,
            0,
            1,
        ),
        (
            "absent_bootstrap",
            CommandClass::Read,
            Phase::Admission,
            Region::Bot,
            5,
            0,
            0,
            0,
            1,
        ),
        (
            "projection_busy",
            CommandClass::WriteFilemarks {
                count: 0,
                immediate: false,
            },
            Phase::Closeout,
            Region::AtOrAfterCursor(14),
            6,
            0,
            0,
            0,
            0,
        ),
        (
            "success",
            CommandClass::WriteFilemarks {
                count: 0,
                immediate: false,
            },
            Phase::Closeout,
            Region::AtOrAfterCursor(14),
            0,
            0,
            0,
            0,
            0,
        ),
        (
            "retired",
            CommandClass::OtherControl,
            Phase::Admission,
            Region::NotTape,
            3,
            0,
            0,
            0,
            0,
        ),
        (
            "sealed",
            CommandClass::OtherControl,
            Phase::Admission,
            Region::NotTape,
            3,
            0,
            0,
            0,
            0,
        ),
        (
            "finalizing",
            CommandClass::OtherControl,
            Phase::Admission,
            Region::NotTape,
            3,
            0,
            0,
            0,
            0,
        ),
        (
            "ingested",
            CommandClass::OtherControl,
            Phase::Admission,
            Region::NotTape,
            3,
            0,
            0,
            0,
            0,
        ),
        (
            "foreign",
            CommandClass::OtherControl,
            Phase::Admission,
            Region::NotTape,
            3,
            0,
            0,
            0,
            0,
        ),
        (
            "no_pool",
            CommandClass::OtherControl,
            Phase::Admission,
            Region::NotTape,
            3,
            0,
            0,
            0,
            0,
        ),
        (
            "pending",
            CommandClass::OtherControl,
            Phase::Admission,
            Region::NotTape,
            3,
            0,
            0,
            0,
            0,
        ),
        (
            "lease",
            CommandClass::OtherControl,
            Phase::Admission,
            Region::NotTape,
            3,
            0,
            0,
            0,
            0,
        ),
        (
            "finalize_lease",
            CommandClass::OtherControl,
            Phase::Admission,
            Region::NotTape,
            3,
            0,
            0,
            0,
            0,
        ),
        (
            "mode_sense",
            CommandClass::Mode,
            Phase::Closeout,
            Region::NotTape,
            2,
            0,
            0,
            0,
            2,
        ),
        (
            "catalog_read",
            CommandClass::OtherControl,
            Phase::Closeout,
            Region::NotTape,
            3,
            0,
            0,
            0,
            0,
        ),
        (
            "post_projection",
            CommandClass::WriteFilemarks {
                count: 0,
                immediate: false,
            },
            Phase::Closeout,
            Region::AtOrAfterCursor(14),
            6,
            0,
            0,
            0,
            0,
        ),
        (
            "summary_mismatch",
            CommandClass::OtherControl,
            Phase::Closeout,
            Region::NotTape,
            3,
            0,
            0,
            0,
            0,
        ),
        (
            "tail",
            CommandClass::Read,
            Phase::Admission,
            Region::AtOrAfterCursor(14),
            5,
            0,
            0,
            0,
            1,
        ),
        (
            "identity",
            CommandClass::Read,
            Phase::Admission,
            Region::Bot,
            5,
            0,
            0,
            0,
            1,
        ),
        (
            "read",
            CommandClass::Read,
            Phase::Closeout,
            Region::Committed(10..14),
            1,
            4,
            17,
            0,
            1,
        ),
        (
            "medium",
            CommandClass::Read,
            Phase::Closeout,
            Region::Committed(10..14),
            1,
            3,
            17,
            0,
            1,
        ),
        (
            "open_read",
            CommandClass::Read,
            Phase::Admission,
            Region::Committed(10..14),
            1,
            4,
            17,
            0,
            1,
        ),
        (
            "open_medium",
            CommandClass::Read,
            Phase::Admission,
            Region::Committed(10..14),
            1,
            3,
            17,
            0,
            1,
        ),
        (
            "missing_checkpoint",
            CommandClass::OtherControl,
            Phase::Closeout,
            Region::NotTape,
            3,
            0,
            0,
            0,
            0,
        ),
        (
            "barcode_fence",
            CommandClass::OtherControl,
            Phase::Admission,
            Region::NotTape,
            3,
            0,
            0,
            0,
            0,
        ),
        (
            "late_barcode_fence",
            CommandClass::OtherControl,
            Phase::Closeout,
            Region::NotTape,
            3,
            0,
            0,
            0,
            0,
        ),
        (
            "write_protected",
            CommandClass::OtherControl,
            Phase::Admission,
            Region::NotTape,
            3,
            0,
            0,
            0,
            0,
        ),
    ];
    let expected_names: std::collections::BTreeSet<_> =
        include_str!("../../../remanence-state/tests/census/w2-trace.tsv")
            .lines()
            .skip(1)
            .filter(|line| !line.is_empty())
            .map(|line| line.split('\t').next().unwrap())
            .collect();
    let names: std::collections::BTreeSet<_> = vectors.iter().map(|v| v.0).collect();
    assert_eq!(vectors.len(), 59);
    assert_eq!(names, expected_names);
    for &(name, class, phase, ref region, kind, key, asc, flags, expected) in vectors {
        let mut evidence = CompletionEvidence::success();
        let mut context = OperationContext {
            phase,
            region: region.clone(),
            ..OperationContext::default()
        };
        match kind {
            0 => {}
            1 => {
                let mut raw = sense(key, asc, 0);
                raw[2] |= flags;
                evidence = CompletionEvidence::from_scsi(&ScsiError::CheckCondition {
                    sense: raw,
                    bytes_transferred: 0,
                });
                if class == CommandClass::Position && matches!(region, Region::AtOrAfterCursor(_)) {
                    context.durability.reproof = Reproof::Contradicted;
                }
            }
            2 => evidence = CompletionEvidence::unknown(),
            3 => {
                evidence.dispatch = Dispatch::NotIssued;
                evidence.failed = true;
            }
            4 => context.durability.authority_append = AuthorityAppend::Ambiguous,
            5 => {
                evidence.fact = CompletionFact::SuffixContradiction;
                evidence.failed = true;
            }
            6 => {
                context.durability.authority_append = AuthorityAppend::Durable;
                context.durability.projection = Projection::Pending;
            }
            _ => unreachable!(),
        }
        let outcome = classify(OutcomeInput {
            class,
            context,
            evidence,
        });
        assert!(
            match expected {
                0 => outcome.effect == Effect::NoFence,
                1 => matches!(outcome.effect, Effect::WriteFence(_)),
                2 => outcome.effect == Effect::Transient,
                _ => false,
            },
            "{name}: {outcome:?}"
        );
    }
}
