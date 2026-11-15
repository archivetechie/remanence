//! Process-wide history-pass accounting for tests and fixtures, including actor
//! threads. Labels retain the initiating file, without line-number churn.
use std::{collections::BTreeMap, sync::Mutex};

/// A full traversal's purpose, independent of its calling file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PassKind {
    /// Replay a library's history from its first segment.
    LibraryReplay,
    /// Validate a tape's captured history from its first frame.
    TapeValidate,
    /// Hash a tape prefix from byte zero.
    TapePrefixHash,
    /// Decode a non-empty tape range starting at its first frame.
    TapeFirstFrame,
}

// Leaf lock: take last, hold only for map updates or copies, never for I/O
// or while acquiring another lock. Poison does not disable measurement.
static COUNTS: Mutex<BTreeMap<(PassKind, &'static str), u64>> = Mutex::new(BTreeMap::new());

/// Record one traversal, attributed through the caller's propagation chain.
#[track_caller]
pub(crate) fn count(kind: PassKind) {
    let file = std::panic::Location::caller().file();
    let mut counts = COUNTS.lock().unwrap_or_else(|error| error.into_inner());
    *counts.entry((kind, file)).or_default() += 1;
}

/// Clear process-wide counts. Call only from an isolated production-path test.
pub fn reset() {
    COUNTS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
}

/// Copy counts in kind/file order, retaining no lock after returning.
pub fn counts() -> Vec<(PassKind, &'static str, u64)> {
    COUNTS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .map(|(&(kind, file), &count)| (kind, file, count))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own(kind: PassKind) -> u64 {
        counts()
            .into_iter()
            .filter(|&(observed, file, _)| observed == kind && file == file!())
            .map(|(_, _, count)| count)
            .sum()
    }

    /// Each public replay entry counts once and preserves this calling file.
    #[test]
    fn library_replay_entries_count_once() {
        use crate::library_log::{authority::LibraryAuthority, store, *};
        struct Clock;
        impl store::Clock for Clock {
            fn now(&self) -> u64 {
                1_793_664_000_000_000_000
            }
        }
        struct Uuids;
        impl store::UuidSource for Uuids {
            fn next(&self, sequence: u64) -> Id {
                let mut id = [0xc0; 16];
                id[8..].copy_from_slice(&sequence.to_be_bytes());
                id
            }
        }
        struct Absent;
        impl TapeView for Absent {
            fn lookup(&self, _: &TapeAt) -> Result<TapeLookup, FoldError> {
                Ok(TapeLookup::NotYet)
            }
        }
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("library");
        let uuid = [0xb1; 16];
        let lease = std::sync::Arc::new(crate::StateLockGuard::acquire(root.path()).unwrap());
        let authority = LibraryAuthority::open(
            &directory,
            uuid,
            store::Origin {
                host: "full-pass".into(),
                pid: 1,
                build: "fixture".into(),
            },
            Box::new(Clock),
            Box::new(Uuids),
            false,
            &Absent,
            lease,
        )
        .unwrap();
        authority
            .drive_observed(
                DriveObserved {
                    drive_uuid: [1; 16],
                    serial: None,
                    identity_source: IdentitySource::Standalone,
                    endpoint: None,
                    managed: Managed::Rem,
                    inventory_generation: 1,
                    vendor: None,
                    product: None,
                    firmware_rev: None,
                },
                AuditActor::System,
                SourceLayer::Layer4,
                &Absent,
            )
            .unwrap();
        drop(authority);

        let before = own(PassKind::LibraryReplay);
        let replay = store::replay(&directory, uuid, &Absent).unwrap();
        assert_eq!(replay.last_sequence, 1, "non-empty library history");
        assert_eq!(
            own(PassKind::LibraryReplay) - before,
            1,
            "store::replay delta"
        );
        let before = own(PassKind::LibraryReplay);
        store::replay_observed(&directory, uuid, &Absent, &mut |_| Ok(())).unwrap();
        assert_eq!(
            own(PassKind::LibraryReplay) - before,
            1,
            "store::replay_observed delta"
        );
    }

    fn delta<T>(kind: PassKind, expected: u64, row: &str, action: impl FnOnce() -> T) -> T {
        let before = own(kind);
        let result = action();
        assert_eq!(own(kind) - before, expected, "{row}");
        result
    }

    /// Count full tape traversals while excluding later groups and empty ranges.
    #[test]
    fn tape_passes_respect_history_boundaries() {
        use crate::authority::{TapeAuthority, TapeAuthorityView, TapeAuthorityWriter};
        use crate::checkpoint::tests::{record, second_record};
        use crate::tape_log::store::Store;
        use PassKind::{TapeFirstFrame, TapePrefixHash, TapeValidate};

        let root = tempfile::tempdir().unwrap();
        let id = [0x67; 16];
        let authority = TapeAuthority::open_fixture(root.path(), id).unwrap();
        let mut lease = authority.acquire_exclusive().unwrap();
        let first = record(id);
        lease.publish_report(&(&first).into()).unwrap();
        let first_snapshot = lease.freeze().unwrap();
        let second_sequence = first_snapshot.raw.summary().last_closed_sequence + 1;
        let second_start = first_snapshot.raw.summary().closed_end;
        let second = second_record(id);
        lease.publish_report(&(&second).into()).unwrap();
        drop(lease);

        let snapshot = delta(TapeValidate, 1, "snapshot validation", || {
            Store::open_fixture(root.path(), id)
                .unwrap()
                .snapshot()
                .unwrap()
        });
        let through = snapshot.summary().last_closed_sequence;
        let closed_end = snapshot.summary().closed_end;
        delta(TapeFirstFrame, 1, "visit_frames", || {
            snapshot.visit_frames(|_| Ok(())).unwrap();
        });
        delta(TapeFirstFrame, 1, "visit_frames_after zero", || {
            snapshot.visit_frames_after(0, 0, |_| Ok(())).unwrap();
        });
        delta(
            TapeFirstFrame,
            0,
            "visit_frames_after closed sequence",
            || {
                snapshot
                    .visit_frames_after(through, closed_end, |_| Ok(()))
                    .unwrap();
            },
        );
        delta(TapeFirstFrame, 1, "record_evidence", || {
            assert!(snapshot.record_evidence(1).unwrap().is_some());
        });
        delta(
            TapePrefixHash,
            1,
            "projection_prefix_digest closed end",
            || {
                snapshot.projection_prefix_digest(closed_end).unwrap();
            },
        );
        delta(TapePrefixHash, 0, "projection_prefix_digest zero", || {
            snapshot.projection_prefix_digest(0).unwrap();
        });
        let second_group = snapshot.group_snapshot(second_sequence, second_start, closed_end);
        let mut group_frames = 0;
        delta(TapeFirstFrame, 0, "second group snapshot", || {
            second_group
                .visit_frames(|_| {
                    group_frames += 1;
                    Ok(())
                })
                .unwrap();
        });
        assert!(group_frames > 0, "second group contains frames");
        let empty = delta(TapeFirstFrame, 1, "before(1) scan", || {
            snapshot.before(1).unwrap()
        });
        delta(TapeFirstFrame, 0, "before(1) empty visit", || {
            empty
                .visit_frames(|_| panic!("before(1) has no frames"))
                .unwrap();
        });
        delta(TapeValidate, 0, "no-log snapshot", || {
            Store::open_fixture(root.path(), [0x68; 16])
                .unwrap()
                .snapshot()
                .unwrap();
        });
    }
}
