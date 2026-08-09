//! Canonical planned layout for the five-file terminal index tail.
//!
//! The terminal close is always replica A, separation AB, replica B,
//! separation BC, and replica C. This module owns the checked physical plan
//! and its digest so codecs, journals, and recovery code do not reconstruct
//! logical positions independently. Conservative filemark capacity charges
//! deliberately live outside this on-media plan.

use sha2::{Digest, Sha256};

/// Number of complete terminal index replicas.
pub const TERMINAL_INDEX_REPLICA_COUNT: u16 = 3;

/// Number of typed separation extents between replicas.
pub const TERMINAL_INDEX_SEPARATION_COUNT: u16 = 2;

/// Number of tape files in the complete terminal tail.
pub const TERMINAL_TAIL_COMPONENT_COUNT: usize = 5;

/// Durable barrier-proved progress through the terminal tail.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TerminalTailProgress {
    /// No terminal component has durable barrier authority.
    #[default]
    BeforeReplicaA,
    /// Replica A is durable.
    AfterReplicaA,
    /// Separation AB is durable.
    AfterSeparationAb,
    /// Replica B is durable.
    AfterReplicaB,
    /// Separation BC is durable.
    AfterSeparationBc,
    /// Replica C is durable; no further terminal component may be appended.
    AfterReplicaC,
}

impl TerminalTailProgress {
    /// Zero-based index of the only component that may be written next.
    pub const fn next_component_index(self) -> Option<usize> {
        match self {
            Self::BeforeReplicaA => Some(0),
            Self::AfterReplicaA => Some(1),
            Self::AfterSeparationAb => Some(2),
            Self::AfterReplicaB => Some(3),
            Self::AfterSeparationBc => Some(4),
            Self::AfterReplicaC => None,
        }
    }

    /// Progress obtained only after the next component's barrier succeeds.
    pub const fn successor(self) -> Option<Self> {
        match self {
            Self::BeforeReplicaA => Some(Self::AfterReplicaA),
            Self::AfterReplicaA => Some(Self::AfterSeparationAb),
            Self::AfterSeparationAb => Some(Self::AfterReplicaB),
            Self::AfterReplicaB => Some(Self::AfterSeparationBc),
            Self::AfterSeparationBc => Some(Self::AfterReplicaC),
            Self::AfterReplicaC => None,
        }
    }

    /// Number of complete replicas represented by this progress.
    pub const fn completed_replicas(self) -> u8 {
        match self {
            Self::BeforeReplicaA => 0,
            Self::AfterReplicaA | Self::AfterSeparationAb => 1,
            Self::AfterReplicaB | Self::AfterSeparationBc => 2,
            Self::AfterReplicaC => 3,
        }
    }
}

/// Fixed record sizes supported by terminal index and separation frames.
pub const TERMINAL_INDEX_BLOCK_SIZES: &[u32] = &[256 * 1024, 512 * 1024, 1024 * 1024];

const TERMINAL_TAIL_LAYOUT_DIGEST_DOMAIN: &[u8] = b"REM-TERMINAL-TAIL-LAYOUT-V1\0";

/// Typed component in the terminal tail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum TerminalTailComponentKind {
    /// Complete terminal inventory replica.
    TapeIndexReplica = 4,
    /// Typed zero-interior physical separation extent.
    IndexSeparationExtent = 5,
}

impl TerminalTailComponentKind {
    /// Stable format discriminator.
    pub const fn code(self) -> u16 {
        self as u16
    }
}

/// One planned filemark-delimited component.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalTailComponentPlan {
    /// Structural kind.
    pub kind: TerminalTailComponentKind,
    /// One-based ordinal within its kind: replicas 1..=3, gaps 1..=2.
    pub ordinal: u16,
    /// Dense tape-file number from BOT.
    pub planned_tape_file_number: u64,
    /// Logical record/filemark coordinate of the component start.
    pub planned_start_lba: u64,
    /// Fixed data records before the trailing filemark.
    pub record_count: u64,
}

impl TerminalTailComponentPlan {
    /// Logical location span, counting the required trailing filemark once.
    pub fn logical_span(self) -> Result<u64, TerminalTailLayoutError> {
        checked_add(
            self.record_count,
            1,
            "component records plus one logical filemark",
        )
    }

    fn expected_next_lba(self) -> Result<u64, TerminalTailLayoutError> {
        checked_add(
            self.planned_start_lba,
            self.logical_span()?,
            "next component start LBA",
        )
    }
}

/// Exact planned five-file layout and expected terminal EOD coordinate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalTailLayout {
    /// SCSI partition containing the tail; draft profile requires partition 0.
    pub partition: u32,
    /// Fixed record size shared by all five components.
    pub block_size: u32,
    /// A, gap AB, B, gap BC, C in that exact order.
    pub components: [TerminalTailComponentPlan; TERMINAL_TAIL_COMPONENT_COUNT],
    /// Expected logical EOD coordinate after C's trailing filemark.
    pub expected_eod_lba: u64,
}

impl TerminalTailLayout {
    /// Construct the unique checked five-file plan.
    pub fn new(
        partition: u32,
        block_size: u32,
        first_tape_file_number: u64,
        first_start_lba: u64,
        replica_records: u64,
        separation_records: u64,
    ) -> Result<Self, TerminalTailLayoutError> {
        if partition != 0 {
            return Err(TerminalTailLayoutError::UnsupportedPartition { partition });
        }
        validate_terminal_index_block_size_hint(block_size)?;
        if replica_records < 2 {
            return Err(TerminalTailLayoutError::InvalidRecordCount {
                kind: TerminalTailComponentKind::TapeIndexReplica,
                records: replica_records,
                minimum: 2,
            });
        }
        if separation_records < 2 {
            return Err(TerminalTailLayoutError::InvalidRecordCount {
                kind: TerminalTailComponentKind::IndexSeparationExtent,
                records: separation_records,
                minimum: 2,
            });
        }
        let specs = [
            (
                TerminalTailComponentKind::TapeIndexReplica,
                1,
                replica_records,
            ),
            (
                TerminalTailComponentKind::IndexSeparationExtent,
                1,
                separation_records,
            ),
            (
                TerminalTailComponentKind::TapeIndexReplica,
                2,
                replica_records,
            ),
            (
                TerminalTailComponentKind::IndexSeparationExtent,
                2,
                separation_records,
            ),
            (
                TerminalTailComponentKind::TapeIndexReplica,
                3,
                replica_records,
            ),
        ];

        let mut file_number = first_tape_file_number;
        let mut start_lba = first_start_lba;
        let mut components = [TerminalTailComponentPlan {
            kind: TerminalTailComponentKind::TapeIndexReplica,
            ordinal: 1,
            planned_tape_file_number: 0,
            planned_start_lba: 0,
            record_count: replica_records,
        }; TERMINAL_TAIL_COMPONENT_COUNT];

        for (index, (kind, ordinal, record_count)) in specs.into_iter().enumerate() {
            let component = TerminalTailComponentPlan {
                kind,
                ordinal,
                planned_tape_file_number: file_number,
                planned_start_lba: start_lba,
                record_count,
            };
            components[index] = component;
            start_lba = component.expected_next_lba()?;
            if index + 1 != TERMINAL_TAIL_COMPONENT_COUNT {
                file_number = checked_add(file_number, 1, "terminal tape-file number")?;
            }
        }

        let layout = Self {
            partition,
            block_size,
            components,
            expected_eod_lba: start_lba,
        };
        layout.validate()?;
        Ok(layout)
    }

    /// Validate order, ordinals, dense file numbers, positions, and EOD.
    pub fn validate(&self) -> Result<(), TerminalTailLayoutError> {
        if self.partition != 0 {
            return Err(TerminalTailLayoutError::UnsupportedPartition {
                partition: self.partition,
            });
        }
        validate_terminal_index_block_size_hint(self.block_size)?;
        let expected = [
            (TerminalTailComponentKind::TapeIndexReplica, 1),
            (TerminalTailComponentKind::IndexSeparationExtent, 1),
            (TerminalTailComponentKind::TapeIndexReplica, 2),
            (TerminalTailComponentKind::IndexSeparationExtent, 2),
            (TerminalTailComponentKind::TapeIndexReplica, 3),
        ];

        for (index, (component, (expected_kind, expected_ordinal))) in
            self.components.iter().zip(expected).enumerate()
        {
            if component.kind != expected_kind || component.ordinal != expected_ordinal {
                return Err(TerminalTailLayoutError::WrongComponent {
                    index,
                    expected_kind,
                    expected_ordinal,
                    actual_kind: component.kind,
                    actual_ordinal: component.ordinal,
                });
            }
            let minimum = 2;
            if component.record_count < minimum {
                return Err(TerminalTailLayoutError::InvalidRecordCount {
                    kind: component.kind,
                    records: component.record_count,
                    minimum,
                });
            }
            if let Some(next) = self.components.get(index + 1) {
                let expected_file = checked_add(
                    component.planned_tape_file_number,
                    1,
                    "dense terminal tape-file number",
                )?;
                if next.planned_tape_file_number != expected_file {
                    return Err(TerminalTailLayoutError::NonDenseTapeFileNumber {
                        index: index + 1,
                        expected: expected_file,
                        actual: next.planned_tape_file_number,
                    });
                }
                let expected_start = component.expected_next_lba()?;
                if next.planned_start_lba != expected_start {
                    return Err(TerminalTailLayoutError::NonContiguousStart {
                        index: index + 1,
                        expected: expected_start,
                        actual: next.planned_start_lba,
                    });
                }
            }
        }

        let expected_replica_records = self.components[0].record_count;
        for component in [self.components[2], self.components[4]] {
            if component.record_count != expected_replica_records {
                return Err(TerminalTailLayoutError::NonUniformRecordCount {
                    kind: component.kind,
                    ordinal: component.ordinal,
                    expected: expected_replica_records,
                    actual: component.record_count,
                });
            }
        }
        let expected_separation_records = self.components[1].record_count;
        let second_separation = self.components[3];
        if second_separation.record_count != expected_separation_records {
            return Err(TerminalTailLayoutError::NonUniformRecordCount {
                kind: second_separation.kind,
                ordinal: second_separation.ordinal,
                expected: expected_separation_records,
                actual: second_separation.record_count,
            });
        }

        let expected_eod =
            self.components[TERMINAL_TAIL_COMPONENT_COUNT - 1].expected_next_lba()?;
        if self.expected_eod_lba != expected_eod {
            return Err(TerminalTailLayoutError::WrongExpectedEod {
                expected: expected_eod,
                actual: self.expected_eod_lba,
            });
        }
        Ok(())
    }

    /// Return one planned replica by one-based ordinal.
    pub fn replica(
        &self,
        ordinal: u16,
    ) -> Result<TerminalTailComponentPlan, TerminalTailLayoutError> {
        self.component(TerminalTailComponentKind::TapeIndexReplica, ordinal)
    }

    /// Return one planned separation extent by one-based ordinal.
    pub fn separation(
        &self,
        ordinal: u16,
    ) -> Result<TerminalTailComponentPlan, TerminalTailLayoutError> {
        self.component(TerminalTailComponentKind::IndexSeparationExtent, ordinal)
    }

    /// SHA-256 over the complete planned layout, never observed future facts.
    pub fn digest(&self) -> Result<[u8; 32], TerminalTailLayoutError> {
        self.validate()?;
        let mut hasher = Sha256::new();
        hasher.update(TERMINAL_TAIL_LAYOUT_DIGEST_DOMAIN);
        hasher.update(self.partition.to_le_bytes());
        hasher.update(self.block_size.to_le_bytes());
        hasher.update((TERMINAL_TAIL_COMPONENT_COUNT as u16).to_le_bytes());
        for component in self.components {
            hasher.update(component.kind.code().to_le_bytes());
            hasher.update(component.ordinal.to_le_bytes());
            hasher.update(1u32.to_le_bytes()); // exactly one trailing filemark
            hasher.update(component.planned_tape_file_number.to_le_bytes());
            hasher.update(component.planned_start_lba.to_le_bytes());
            hasher.update(component.record_count.to_le_bytes());
        }
        hasher.update(self.expected_eod_lba.to_le_bytes());
        Ok(hasher.finalize().into())
    }

    fn component(
        &self,
        kind: TerminalTailComponentKind,
        ordinal: u16,
    ) -> Result<TerminalTailComponentPlan, TerminalTailLayoutError> {
        self.validate()?;
        self.components
            .iter()
            .copied()
            .find(|component| component.kind == kind && component.ordinal == ordinal)
            .ok_or(TerminalTailLayoutError::MissingComponent { kind, ordinal })
    }
}

/// Validate a terminal control-file block size and return it as `usize`.
pub fn validate_terminal_index_block_size(
    block_size: u32,
) -> Result<usize, TerminalTailLayoutError> {
    if !TERMINAL_INDEX_BLOCK_SIZES.contains(&block_size) {
        return Err(TerminalTailLayoutError::UnsupportedBlockSize { block_size });
    }
    usize::try_from(block_size)
        .map_err(|_| TerminalTailLayoutError::UnsupportedBlockSize { block_size })
}

/// Validate a physically hinted terminal record size for generic decoding.
///
/// Readers accept any checked fixed record large enough for the framing table;
/// production writers and capacity planning remain restricted to
/// [`TERMINAL_INDEX_BLOCK_SIZES`] through [`validate_terminal_index_block_size`].
pub(crate) fn validate_terminal_index_block_size_hint(
    block_size: u32,
) -> Result<usize, TerminalTailLayoutError> {
    if block_size < 0x400 {
        return Err(TerminalTailLayoutError::UnsupportedBlockSize { block_size });
    }
    usize::try_from(block_size)
        .map_err(|_| TerminalTailLayoutError::UnsupportedBlockSize { block_size })
}

/// Checked terminal-tail planning failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TerminalTailLayoutError {
    /// Draft terminal-tail profile supports partition zero only.
    #[error("unsupported terminal-tail partition {partition}")]
    UnsupportedPartition {
        /// Supplied partition number.
        partition: u32,
    },
    /// Fixed-block size is outside the draft terminal-index profile.
    #[error("unsupported terminal-index block size {block_size}")]
    UnsupportedBlockSize {
        /// Supplied bytes per fixed record.
        block_size: u32,
    },
    /// A component cannot hold its mandatory header and footer.
    #[error("{kind:?} record count {records} is below minimum {minimum}")]
    InvalidRecordCount {
        /// Component kind.
        kind: TerminalTailComponentKind,
        /// Supplied records.
        records: u64,
        /// Required minimum.
        minimum: u64,
    },
    /// Components of the same kind must use one canonical record geometry.
    #[error(
        "{kind:?} component {ordinal} record count {actual}, expected canonical count {expected}"
    )]
    NonUniformRecordCount {
        /// Component kind.
        kind: TerminalTailComponentKind,
        /// One-based ordinal within the kind.
        ordinal: u16,
        /// Record count established by the first component of this kind.
        expected: u64,
        /// Conflicting record count.
        actual: u64,
    },
    /// Checked arithmetic overflowed.
    #[error("terminal-tail arithmetic overflow: {context}")]
    ArithmeticOverflow {
        /// Operation that overflowed.
        context: &'static str,
    },
    /// Component order or ordinal differs from the fixed grammar.
    #[error(
        "terminal component {index} expected {expected_kind:?}/{expected_ordinal}, got {actual_kind:?}/{actual_ordinal}"
    )]
    WrongComponent {
        /// Zero-based component index.
        index: usize,
        /// Expected kind.
        expected_kind: TerminalTailComponentKind,
        /// Expected one-based ordinal.
        expected_ordinal: u16,
        /// Actual kind.
        actual_kind: TerminalTailComponentKind,
        /// Actual one-based ordinal.
        actual_ordinal: u16,
    },
    /// Tape-file numbers are not dense.
    #[error("terminal component {index} tape-file number {actual}, expected {expected}")]
    NonDenseTapeFileNumber {
        /// Zero-based component index.
        index: usize,
        /// Expected number.
        expected: u64,
        /// Actual number.
        actual: u64,
    },
    /// Planned starts are not contiguous in the logical layout coordinate.
    #[error("terminal component {index} start LBA {actual}, expected {expected}")]
    NonContiguousStart {
        /// Zero-based component index.
        index: usize,
        /// Expected start.
        expected: u64,
        /// Actual start.
        actual: u64,
    },
    /// Expected EOD disagrees with the last component charge.
    #[error("terminal expected EOD {actual}, expected {expected}")]
    WrongExpectedEod {
        /// Derived EOD.
        expected: u64,
        /// Supplied EOD.
        actual: u64,
    },
    /// Requested ordinal does not exist in the fixed grammar.
    #[error("terminal layout has no {kind:?} ordinal {ordinal}")]
    MissingComponent {
        /// Component kind.
        kind: TerminalTailComponentKind,
        /// Requested ordinal.
        ordinal: u16,
    },
}

fn checked_add(
    left: u64,
    right: u64,
    context: &'static str,
) -> Result<u64, TerminalTailLayoutError> {
    left.checked_add(right)
        .ok_or(TerminalTailLayoutError::ArithmeticOverflow { context })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_exact_a_gap_b_gap_c_layout() {
        let layout = TerminalTailLayout::new(0, 256 * 1024, 40, 1_000, 12, 4_096).unwrap();

        assert_eq!(
            layout
                .components
                .map(|component| (component.kind, component.ordinal)),
            [
                (TerminalTailComponentKind::TapeIndexReplica, 1),
                (TerminalTailComponentKind::IndexSeparationExtent, 1),
                (TerminalTailComponentKind::TapeIndexReplica, 2),
                (TerminalTailComponentKind::IndexSeparationExtent, 2),
                (TerminalTailComponentKind::TapeIndexReplica, 3),
            ]
        );
        assert_eq!(
            layout
                .components
                .map(|component| component.planned_tape_file_number),
            [40, 41, 42, 43, 44]
        );
        assert_eq!(
            layout
                .components
                .map(|component| component.planned_start_lba),
            [1_000, 1_013, 5_110, 5_123, 9_220]
        );
        assert_eq!(layout.expected_eod_lba, 9_233);
        assert_eq!(layout.replica(3).unwrap(), layout.components[4]);
        assert_eq!(layout.separation(2).unwrap(), layout.components[3]);
    }

    #[test]
    fn digest_changes_when_any_planned_fact_changes() {
        let layout = TerminalTailLayout::new(0, 256 * 1024, 40, 1_000, 12, 4_096).unwrap();
        let digest = layout.digest().unwrap();
        let mut changed = layout;
        for component in &mut changed.components {
            component.planned_tape_file_number += 1;
        }

        assert_ne!(changed.digest().unwrap(), digest);
    }

    #[test]
    fn rejects_nonuniform_replica_or_separation_geometry() {
        let layout = TerminalTailLayout::new(0, 256 * 1024, 40, 1_000, 12, 4_096).unwrap();
        for component_index in [2, 3, 4] {
            let mut changed = layout;
            changed.components[component_index].record_count += 1;
            for later in component_index + 1..TERMINAL_TAIL_COMPONENT_COUNT {
                changed.components[later].planned_start_lba += 1;
            }
            changed.expected_eod_lba += 1;
            assert!(matches!(
                changed.validate(),
                Err(TerminalTailLayoutError::NonUniformRecordCount { .. })
            ));
        }
    }

    #[test]
    fn rejects_double_counted_or_missing_framing_geometry() {
        assert!(matches!(
            TerminalTailLayout::new(0, 256 * 1024, 0, 0, 1, 2),
            Err(TerminalTailLayoutError::InvalidRecordCount {
                kind: TerminalTailComponentKind::TapeIndexReplica,
                ..
            })
        ));
        assert!(matches!(
            TerminalTailLayout::new(0, 256 * 1024, 0, 0, 2, 1),
            Err(TerminalTailLayoutError::InvalidRecordCount {
                kind: TerminalTailComponentKind::IndexSeparationExtent,
                ..
            })
        ));
    }

    #[test]
    fn overflow_fails_closed() {
        assert!(matches!(
            TerminalTailLayout::new(0, 256 * 1024, u64::MAX, 0, 2, 2),
            Err(TerminalTailLayoutError::ArithmeticOverflow { .. })
        ));
        assert!(matches!(
            TerminalTailLayout::new(0, 256 * 1024, 0, u64::MAX, 2, 2),
            Err(TerminalTailLayoutError::ArithmeticOverflow { .. })
        ));
    }

    #[test]
    fn terminal_block_sizes_are_closed_and_checked() {
        for block_size in TERMINAL_INDEX_BLOCK_SIZES {
            assert_eq!(
                validate_terminal_index_block_size(*block_size).unwrap(),
                *block_size as usize
            );
        }
        assert!(matches!(
            validate_terminal_index_block_size(4_096),
            Err(TerminalTailLayoutError::UnsupportedBlockSize { .. })
        ));
    }
}
