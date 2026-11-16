//! Classifier cases pin conservative data evidence and terminal exclusions independently.
use super::*;
use crate::tape_log::{SessionCursor, SessionDrive, SessionOpened};

#[test]
fn classification_table() {
    use PoolRuleEvidence::{Blank, Skip, Unknown, Written};
    let blank = TapeSummary::default();
    let mut written = blank.clone();
    written.object_count = 1;
    let mut sealed = written.clone();
    sealed.sealed = true;
    let mut seal_due = written.clone();
    seal_due.seal_due = Some(crate::tape_log::TerminalFinalizationTrigger::OperatorCloseOut);
    let mut open = blank.clone();
    open.open_session = Some(SessionOpened {
        session_id: [9; 16],
        incarnation: 1,
        base_media_sequence: 1,
        cursor: SessionCursor {
            partition: 0,
            lba: 2,
            next_file: 1,
        },
        opened_at_utc: "2026-11-02T00:00:00Z".into(),
        drive: SessionDrive {
            serial: "pool-rules-test".into(),
            vendor: None,
            product: None,
            endpoint: lib::DriveKey::Standalone(lib::Standalone {
                drive_serial: "pool-rules-test".into(),
            }),
            drive_uuid: None,
        },
    });
    for (name, summary, dropped, row, terminal, recovery, copies, session, expected) in [
        (
            "no log", None, false, true, false, false, false, false, Blank,
        ),
        (
            "empty log",
            Some(&blank),
            false,
            true,
            false,
            false,
            false,
            false,
            Blank,
        ),
        (
            "written",
            Some(&written),
            false,
            true,
            false,
            false,
            false,
            false,
            Written,
        ),
        (
            "sealed written",
            Some(&sealed),
            false,
            true,
            false,
            false,
            false,
            false,
            Skip,
        ),
        (
            "seal due written",
            Some(&seal_due),
            false,
            true,
            false,
            false,
            false,
            false,
            Skip,
        ),
        (
            "terminal written",
            Some(&written),
            false,
            true,
            true,
            false,
            false,
            false,
            Skip,
        ),
        (
            "recovery written",
            Some(&written),
            false,
            true,
            false,
            true,
            false,
            false,
            Skip,
        ),
        (
            "dropped", None, true, true, false, false, false, false, Unknown,
        ),
        (
            "no row", None, false, false, false, false, false, false, Unknown,
        ),
        (
            "copies", None, false, true, false, false, true, false, Unknown,
        ),
        (
            "catalogue session",
            None,
            false,
            true,
            false,
            false,
            false,
            true,
            Unknown,
        ),
        (
            "native session",
            Some(&open),
            false,
            true,
            false,
            false,
            false,
            false,
            Unknown,
        ),
    ] {
        assert_eq!(
            classify_pool_rule_evidence(summary, dropped, row, terminal, recovery, copies, session),
            expected,
            "{name}"
        );
    }
}
