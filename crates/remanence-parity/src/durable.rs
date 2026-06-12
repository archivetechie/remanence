//! Durable tape-file boundary tracking for Layer 3c writer paths.
//!
//! A tape file becomes catalog-visible only after its fixed blocks, trailing
//! synchronous filemark, post-barrier position capture, and catalog transaction
//! all succeed. This helper keeps that commit-point rule in one place for the
//! normal writer and the resume-generated sidecar writer.

use crate::error::ParityError;
use crate::filemark_map::{FilemarkMap, ScopedFilemarkMap, TapeFileKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OutstandingTapeFileCommit {
    kind: TapeFileKind,
    tape_file_number: u32,
    started_after_tape_file_number: Option<u32>,
}

/// Tracks the last tape-file boundary that is safe to expose to the catalog.
///
/// The writer may have at most one tape file in flight: an object, a parity
/// sidecar, or a bootstrap. Hard EOM and completion-unknown failures abandon
/// that in-flight file and leave the state pointing at the previous committed
/// boundary; only a successful synchronous filemark plus map/catalog handoff
/// advances `last_committed_tape_file_number`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DurableBoundaryState {
    last_committed_tape_file_number: Option<u32>,
    outstanding: Option<OutstandingTapeFileCommit>,
}

impl DurableBoundaryState {
    /// Start at BOT with no catalog-committed tape files.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Seed from a committed filemark-map prefix.
    pub(crate) fn from_committed_prefix(prefix: &FilemarkMap) -> Self {
        Self::from_last_committed_tape_file_number(
            prefix.entries().last().map(|entry| entry.tape_file_number),
        )
    }

    /// Seed the read-side boundary from a scoped catalog/bootstrap map.
    ///
    /// Complete maps expose every described tape file as committed. Prefix
    /// maps expose only the authenticated leading tape files; any suffix rows
    /// are forensic navigation and must not drive parity recovery.
    pub(crate) fn from_scoped_map(scoped_map: &ScopedFilemarkMap) -> Result<Self, ParityError> {
        let last_committed_tape_file_number = match scoped_map.validated_prefix_tape_files {
            None => scoped_map
                .map
                .entries()
                .last()
                .map(|entry| entry.tape_file_number),
            Some(0) => None,
            Some(prefix_tape_files) => {
                let expected = prefix_tape_files - 1;
                let index = usize::try_from(expected).map_err(|_| {
                    ParityError::FilemarkMapReconstruct(format!(
                        "validated prefix tape file {expected} does not fit usize"
                    ))
                })?;
                let entry = scoped_map.map.entries().get(index).ok_or_else(|| {
                    ParityError::FilemarkMapReconstruct(format!(
                        "validated prefix tape_file_count {prefix_tape_files} exceeds map length {}",
                        scoped_map.map.entries().len()
                    ))
                })?;
                if entry.tape_file_number != expected {
                    return Err(ParityError::FilemarkMapReconstruct(format!(
                        "validated prefix expected tape file {expected}, got {}",
                        entry.tape_file_number
                    )));
                }
                Some(entry.tape_file_number)
            }
        };

        Ok(Self::from_last_committed_tape_file_number(
            last_committed_tape_file_number,
        ))
    }

    /// Seed from the last catalog-committed tape-file number.
    pub(crate) fn from_last_committed_tape_file_number(
        last_committed_tape_file_number: Option<u32>,
    ) -> Self {
        Self {
            last_committed_tape_file_number,
            outstanding: None,
        }
    }

    /// Mark one tape file as started but not yet catalog-durable.
    pub(crate) fn begin_tape_file(
        &mut self,
        kind: TapeFileKind,
        tape_file_number: u32,
    ) -> Result<(), ParityError> {
        if self.outstanding.is_some() {
            return Err(ParityError::Invariant(
                "cannot start a tape file while another tape file is not durable",
            ));
        }
        if let Some(last_committed) = self.last_committed_tape_file_number {
            let expected = last_committed.checked_add(1).ok_or(ParityError::Invariant(
                "durable boundary tape-file number overflow",
            ))?;
            if tape_file_number != expected {
                return Err(ParityError::Invariant(
                    "next tape file must follow the last durable boundary",
                ));
            }
        } else if tape_file_number != 0 {
            return Err(ParityError::Invariant(
                "first tape file must start at durable boundary zero",
            ));
        }
        self.outstanding = Some(OutstandingTapeFileCommit {
            kind,
            tape_file_number,
            started_after_tape_file_number: self.last_committed_tape_file_number,
        });
        Ok(())
    }

    /// Promote the outstanding tape file to the last catalog-durable boundary.
    pub(crate) fn commit_tape_file(
        &mut self,
        kind: TapeFileKind,
        tape_file_number: u32,
    ) -> Result<(), ParityError> {
        self.expect_outstanding(kind, tape_file_number)?;
        self.last_committed_tape_file_number = Some(tape_file_number);
        self.outstanding = None;
        Ok(())
    }

    /// Abandon the outstanding tape file and roll back to the prior boundary.
    pub(crate) fn abandon_tape_file(
        &mut self,
        kind: TapeFileKind,
        tape_file_number: u32,
    ) -> Result<Option<u32>, ParityError> {
        let outstanding = self.expect_outstanding(kind, tape_file_number)?;
        self.last_committed_tape_file_number = outstanding.started_after_tape_file_number;
        self.outstanding = None;
        Ok(self.last_committed_tape_file_number)
    }

    #[cfg(test)]
    fn last_committed_tape_file_number(&self) -> Option<u32> {
        self.last_committed_tape_file_number
    }

    /// Whether a tape file sits at or before the durable committed boundary.
    pub(crate) fn contains_committed_tape_file(&self, tape_file_number: u32) -> bool {
        self.last_committed_tape_file_number
            .is_some_and(|last_committed| tape_file_number <= last_committed)
    }

    #[cfg(test)]
    fn outstanding(&self) -> Option<OutstandingTapeFileCommit> {
        self.outstanding
    }

    fn expect_outstanding(
        &self,
        kind: TapeFileKind,
        tape_file_number: u32,
    ) -> Result<OutstandingTapeFileCommit, ParityError> {
        let Some(outstanding) = self.outstanding else {
            return Err(ParityError::Invariant(
                "no outstanding tape file at the durable boundary",
            ));
        };
        if outstanding.kind != kind || outstanding.tape_file_number != tape_file_number {
            return Err(ParityError::Invariant(
                "outstanding tape file does not match the durable-boundary transition",
            ));
        }
        Ok(outstanding)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filemark_map::{MapScope, TapeFileMapEntry};

    #[test]
    fn durable_boundary_abandons_failed_file_to_last_committed_tape_file() {
        let mut boundary = DurableBoundaryState::new();

        boundary
            .begin_tape_file(TapeFileKind::Bootstrap, 0)
            .expect("first bootstrap can start at BOT");
        boundary
            .commit_tape_file(TapeFileKind::Bootstrap, 0)
            .expect("bootstrap filemark commits boundary 0");
        assert_eq!(boundary.last_committed_tape_file_number(), Some(0));
        assert_eq!(boundary.outstanding(), None);

        boundary
            .begin_tape_file(TapeFileKind::Object, 1)
            .expect("object starts after committed bootstrap");
        assert_eq!(
            boundary.outstanding(),
            Some(OutstandingTapeFileCommit {
                kind: TapeFileKind::Object,
                tape_file_number: 1,
                started_after_tape_file_number: Some(0),
            })
        );

        let rolled_back_to = boundary
            .abandon_tape_file(TapeFileKind::Object, 1)
            .expect("EOM rolls back to previous durable boundary");
        assert_eq!(rolled_back_to, Some(0));
        assert_eq!(boundary.last_committed_tape_file_number(), Some(0));
        assert_eq!(boundary.outstanding(), None);

        boundary
            .begin_tape_file(TapeFileKind::Object, 1)
            .expect("same tape-file number can be retried after rollback");
        boundary
            .commit_tape_file(TapeFileKind::Object, 1)
            .expect("retried object commits");
        assert_eq!(boundary.last_committed_tape_file_number(), Some(1));
    }

    #[test]
    fn durable_boundary_rejects_nested_or_gap_commitments() {
        let mut boundary = DurableBoundaryState::new();

        let gap = boundary
            .begin_tape_file(TapeFileKind::Object, 1)
            .expect_err("first tape file must be numbered from BOT");
        assert!(
            matches!(gap, ParityError::Invariant(message) if message.contains("first tape file"))
        );

        boundary
            .begin_tape_file(TapeFileKind::Bootstrap, 0)
            .expect("bootstrap starts");
        let nested = boundary
            .begin_tape_file(TapeFileKind::Object, 1)
            .expect_err("cannot start another file before the filemark barrier");
        assert!(
            matches!(nested, ParityError::Invariant(message) if message.contains("another tape file"))
        );

        let wrong_kind = boundary
            .commit_tape_file(TapeFileKind::Object, 0)
            .expect_err("commit kind must match the outstanding tape file");
        assert!(
            matches!(wrong_kind, ParityError::Invariant(message) if message.contains("does not match"))
        );

        boundary
            .commit_tape_file(TapeFileKind::Bootstrap, 0)
            .expect("bootstrap commits");
        let skipped = boundary
            .begin_tape_file(TapeFileKind::ParitySidecar, 2)
            .expect_err("next file must follow the last durable boundary");
        assert!(
            matches!(skipped, ParityError::Invariant(message) if message.contains("next tape file"))
        );
    }

    #[test]
    fn durable_boundary_can_seed_resume_append_after_committed_prefix() {
        let prefix = FilemarkMap::new(vec![
            crate::filemark_map::TapeFileMapEntry::bootstrap(0, 1),
            crate::filemark_map::TapeFileMapEntry::object(1, 4, 0),
            crate::filemark_map::TapeFileMapEntry::parity_sidecar(2, 6, 0, 0, 4),
        ])
        .expect("prefix validates");
        let mut boundary = DurableBoundaryState::from_committed_prefix(&prefix);

        boundary
            .begin_tape_file(TapeFileKind::ParitySidecar, 3)
            .expect("resume sidecar starts immediately after committed prefix");
        boundary
            .commit_tape_file(TapeFileKind::ParitySidecar, 3)
            .expect("resume sidecar commits");
        assert_eq!(boundary.last_committed_tape_file_number(), Some(3));
    }

    #[test]
    fn durable_boundary_seeds_read_side_from_scoped_prefix() {
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, 4, 0),
            TapeFileMapEntry::parity_sidecar(2, 6, 0, 0, 4),
            TapeFileMapEntry::object(3, 2, 4),
        ])
        .expect("map validates");
        let scoped = ScopedFilemarkMap {
            map: map.clone(),
            validated_prefix_tape_files: Some(3),
            scope: MapScope::Prefix {
                map_total_data_ordinals: 4,
                highest_protected_ordinal: 4,
            },
        };
        let boundary = DurableBoundaryState::from_scoped_map(&scoped)
            .expect("prefix boundary derives from scoped map");

        assert_eq!(boundary.last_committed_tape_file_number(), Some(2));
        assert!(boundary.contains_committed_tape_file(2));
        assert!(!boundary.contains_committed_tape_file(3));

        let complete = ScopedFilemarkMap::from_catalog(map.clone(), 4);
        let complete_boundary = DurableBoundaryState::from_scoped_map(&complete)
            .expect("complete boundary derives from full map");
        assert_eq!(complete_boundary.last_committed_tape_file_number(), Some(3));
        assert!(complete_boundary.contains_committed_tape_file(3));

        let zero_prefix = ScopedFilemarkMap {
            map,
            validated_prefix_tape_files: Some(0),
            scope: MapScope::Prefix {
                map_total_data_ordinals: 0,
                highest_protected_ordinal: 0,
            },
        };
        let zero_boundary = DurableBoundaryState::from_scoped_map(&zero_prefix)
            .expect("zero prefix derives an empty durable boundary");
        assert_eq!(zero_boundary.last_committed_tape_file_number(), None);
        assert!(!zero_boundary.contains_committed_tape_file(0));
    }
}
