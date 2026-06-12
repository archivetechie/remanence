//! Capacity-reserve math for Layer 3c v0.4.4.
//!
//! `begin_object(projected_size_blocks)` must prove, before the first object
//! block is written, that the remaining tape can hold the projected object
//! plus the sidecars, filemarks, bootstraps, and safety margin that object can
//! make necessary. It must also separately prove that local parity spool space
//! can hold sidecar bytes completed by the projected object. Keeping this math
//! as a pure helper gives the writer and catalog tests one place to verify the
//! `TapeCapacity` versus `ParitySpoolCapacity` remedies.

use crate::error::ParityError;

/// Reason a Layer 3c object-start capacity reservation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityReserveCause {
    /// Not enough remaining tape capacity for the projected object, its
    /// trailing filemark, pending sidecars, final sidecar, remaining
    /// bootstraps, and safety margin.
    TapeCapacity,
    /// Not enough local disk capacity for parity sidecar bytes that must be
    /// staged before they can be emitted.
    ParitySpoolCapacity,
}

impl CapacityReserveCause {
    /// Operator remedy for this reserve failure.
    pub fn remedy(self) -> CapacityReserveRemedy {
        match self {
            Self::TapeCapacity => CapacityReserveRemedy::CloseTapeAndRetryOnAnotherTape,
            Self::ParitySpoolCapacity => CapacityReserveRemedy::FreeOrIncreaseParitySpool,
        }
    }
}

/// Layer 5 action required after a capacity-reserve failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityReserveRemedy {
    /// Close the current tape cleanly and retry the whole object on another
    /// tape. Layer 3c never spans one object across tapes.
    CloseTapeAndRetryOnAnotherTape,
    /// Free or enlarge the local parity spool and retry on the same tape;
    /// changing tapes does not address this failure.
    FreeOrIncreaseParitySpool,
}

/// Inputs to the Layer 3c §7.5 object-start reserve calculation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapacityReserveInput {
    /// Conservative upper bound for this object's fixed-size body blocks.
    pub projected_object_blocks: u64,
    /// Fixed tape block size in bytes.
    pub block_size_bytes: u64,
    /// Object-data shards already accumulated in the currently open epoch.
    pub current_epoch_fill_blocks: u64,
    /// Object-data shards in a full epoch (`S * k`).
    pub data_shards_per_epoch: u64,
    /// Raw parity shards in a full sidecar (`S * m`).
    pub parity_shards_per_epoch: u64,
    /// Header/index blocks for one sidecar tape file.
    pub sidecar_index_block_count: u64,
    /// Estimated tape blocks consumed by one object trailing filemark.
    pub object_filemark_blocks: u64,
    /// Estimated tape blocks consumed by one sidecar trailing filemark.
    pub sidecar_filemark_blocks: u64,
    /// Estimated tape blocks consumed by one bootstrap trailing filemark.
    pub bootstrap_filemark_blocks: u64,
    /// Completed sidecars already pending before this object starts.
    pub pending_completed_sidecars: u64,
    /// Number of bootstrap tape files still reserved for this write session.
    pub remaining_bootstrap_count: u64,
    /// Additional tape blocks held back by writer policy.
    pub safety_margin_blocks: u64,
    /// Tape blocks remaining from the current physical position to the
    /// writer's usable capacity limit.
    pub remaining_tape_blocks: u64,
    /// Usable tape blocks on a freshly loaded empty tape under the same
    /// capacity policy. This lets the preflight distinguish "close this tape
    /// and retry" from "this object cannot be written to any single v1 tape".
    pub empty_tape_usable_blocks: u64,
    /// Sidecar bytes already staged in local parity spool.
    pub pending_completed_epoch_parity_bytes: u64,
    /// Local spool bytes available to this write session.
    pub remaining_spool_bytes: u64,
}

/// Successful result of a capacity-reserve calculation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapacityReserveReport {
    /// Number of full epochs completed by the projected object.
    pub epochs_completed_by_object: u64,
    /// Whether the state after this object would require a final partial
    /// sidecar at `finish()`.
    pub final_partial_sidecar_needed: bool,
    /// Tape blocks in one sidecar tape file, including primary/tail
    /// header/index copies, the footer locator, parity shards, and trailing
    /// filemark estimate.
    pub sidecar_tape_file_blocks: u64,
    /// Tape blocks in one bootstrap tape file, including its trailing
    /// filemark estimate.
    pub bootstrap_tape_file_blocks: u64,
    /// Non-object reserve blocks required after admitting the object.
    pub reserve_after_object_blocks: u64,
    /// Total tape blocks needed: projected object blocks plus the reserve.
    pub required_tape_blocks: u64,
    /// Local spool bytes needed after admitting the object.
    pub required_spool_bytes: u64,
}

impl CapacityReserveInput {
    /// Evaluate the object-start reserve.
    pub fn evaluate(self) -> Result<CapacityReserveReport, ParityError> {
        if self.block_size_bytes == 0 {
            return Err(ParityError::Invariant(
                "capacity reserve block size is zero",
            ));
        }
        if self.data_shards_per_epoch == 0 {
            return Err(ParityError::Invariant(
                "capacity reserve data_shards_per_epoch is zero",
            ));
        }
        if self.current_epoch_fill_blocks >= self.data_shards_per_epoch {
            return Err(ParityError::Invariant(
                "capacity reserve current epoch fill is outside the open epoch",
            ));
        }

        let sidecar_metadata_blocks = checked_add(
            checked_mul(2, self.sidecar_index_block_count)?,
            1, // footer locator
        )?;
        let sidecar_tape_file_blocks = checked_sum(&[
            sidecar_metadata_blocks,
            self.parity_shards_per_epoch,
            self.sidecar_filemark_blocks,
        ])?;
        let bootstrap_tape_file_blocks = checked_add(
            self.block_count_per_bootstrap(),
            self.bootstrap_filemark_blocks,
        )?;

        let projected_epoch_fill =
            checked_add(self.current_epoch_fill_blocks, self.projected_object_blocks)?;
        let epochs_completed_by_object = projected_epoch_fill / self.data_shards_per_epoch;
        let final_partial_sidecar_needed = projected_epoch_fill % self.data_shards_per_epoch != 0;

        let pending_sidecar_blocks =
            checked_mul(self.pending_completed_sidecars, sidecar_tape_file_blocks)?;
        let completed_by_object_sidecar_blocks =
            checked_mul(epochs_completed_by_object, sidecar_tape_file_blocks)?;
        let final_partial_sidecar_blocks = if final_partial_sidecar_needed {
            sidecar_tape_file_blocks
        } else {
            0
        };
        let remaining_bootstrap_blocks =
            checked_mul(self.remaining_bootstrap_count, bootstrap_tape_file_blocks)?;

        let reserve_after_object_blocks = checked_sum(&[
            self.object_filemark_blocks,
            pending_sidecar_blocks,
            completed_by_object_sidecar_blocks,
            final_partial_sidecar_blocks,
            remaining_bootstrap_blocks,
            self.safety_margin_blocks,
        ])?;
        let required_tape_blocks =
            checked_add(self.projected_object_blocks, reserve_after_object_blocks)?;

        if self.empty_tape_usable_blocks < required_tape_blocks {
            return Err(ParityError::ObjectTooLargeForEmptyTape {
                projected_object_blocks: self.projected_object_blocks,
                empty_tape_usable_blocks: self.empty_tape_usable_blocks,
                required_reserve_blocks: reserve_after_object_blocks,
            });
        }

        if self.remaining_tape_blocks < required_tape_blocks {
            return Err(ParityError::CapacityReserveExceeded {
                cause: CapacityReserveCause::TapeCapacity,
                projected_object_blocks: self.projected_object_blocks,
                remaining_blocks: Some(self.remaining_tape_blocks),
                reserve_blocks: Some(reserve_after_object_blocks),
                remaining_spool_bytes: None,
                required_spool_bytes: None,
            });
        }

        let sidecar_tape_file_bytes = checked_mul(sidecar_tape_file_blocks, self.block_size_bytes)?;
        let completed_by_object_spool_bytes =
            checked_mul(epochs_completed_by_object, sidecar_tape_file_bytes)?;
        let required_spool_bytes = checked_add(
            self.pending_completed_epoch_parity_bytes,
            completed_by_object_spool_bytes,
        )?;

        if self.remaining_spool_bytes < required_spool_bytes {
            return Err(ParityError::CapacityReserveExceeded {
                cause: CapacityReserveCause::ParitySpoolCapacity,
                projected_object_blocks: self.projected_object_blocks,
                remaining_blocks: None,
                reserve_blocks: None,
                remaining_spool_bytes: Some(self.remaining_spool_bytes),
                required_spool_bytes: Some(required_spool_bytes),
            });
        }

        Ok(CapacityReserveReport {
            epochs_completed_by_object,
            final_partial_sidecar_needed,
            sidecar_tape_file_blocks,
            bootstrap_tape_file_blocks,
            reserve_after_object_blocks,
            required_tape_blocks,
            required_spool_bytes,
        })
    }

    fn block_count_per_bootstrap(&self) -> u64 {
        1
    }
}

fn checked_add(a: u64, b: u64) -> Result<u64, ParityError> {
    a.checked_add(b).ok_or(ParityError::Invariant(
        "capacity reserve arithmetic overflow",
    ))
}

fn checked_mul(a: u64, b: u64) -> Result<u64, ParityError> {
    a.checked_mul(b).ok_or(ParityError::Invariant(
        "capacity reserve arithmetic overflow",
    ))
}

fn checked_sum(values: &[u64]) -> Result<u64, ParityError> {
    values.iter().copied().try_fold(0u64, checked_add)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_input() -> CapacityReserveInput {
        CapacityReserveInput {
            projected_object_blocks: 20,
            block_size_bytes: 1024,
            current_epoch_fill_blocks: 5,
            data_shards_per_epoch: 12,
            parity_shards_per_epoch: 6,
            sidecar_index_block_count: 2,
            object_filemark_blocks: 1,
            sidecar_filemark_blocks: 1,
            bootstrap_filemark_blocks: 1,
            pending_completed_sidecars: 1,
            remaining_bootstrap_count: 2,
            safety_margin_blocks: 3,
            remaining_tape_blocks: 76,
            empty_tape_usable_blocks: u64::MAX,
            pending_completed_epoch_parity_bytes: 7 * 1024,
            remaining_spool_bytes: 31 * 1024,
        }
    }

    #[test]
    fn reserve_counts_object_sidecars_bootstraps_and_margin() {
        let report = sample_input().evaluate().expect("reserve fits");

        assert_eq!(report.epochs_completed_by_object, 2);
        assert!(report.final_partial_sidecar_needed);
        assert_eq!(report.sidecar_tape_file_blocks, 12);
        assert_eq!(report.bootstrap_tape_file_blocks, 2);
        assert_eq!(report.reserve_after_object_blocks, 56);
        assert_eq!(report.required_tape_blocks, 76);
        assert_eq!(report.required_spool_bytes, 31 * 1024);
    }

    #[test]
    fn tape_shortfall_reports_tape_capacity_before_object_starts() {
        let input = CapacityReserveInput {
            remaining_tape_blocks: 75,
            ..sample_input()
        };
        let err = input.evaluate().expect_err("tape should be short");

        match err {
            ParityError::CapacityReserveExceeded {
                cause,
                projected_object_blocks,
                remaining_blocks,
                reserve_blocks,
                remaining_spool_bytes,
                required_spool_bytes,
            } => {
                assert_eq!(cause, CapacityReserveCause::TapeCapacity);
                assert_eq!(projected_object_blocks, 20);
                assert_eq!(remaining_blocks, Some(75));
                assert_eq!(reserve_blocks, Some(56));
                assert_eq!(remaining_spool_bytes, None);
                assert_eq!(required_spool_bytes, None);
            }
            other => panic!("expected capacity reserve error, got {other:?}"),
        }
    }

    #[test]
    fn object_too_large_for_empty_tape_is_distinct_from_current_tape_shortfall() {
        let input = CapacityReserveInput {
            empty_tape_usable_blocks: 75,
            remaining_tape_blocks: 75,
            ..sample_input()
        };
        let err = input
            .evaluate()
            .expect_err("object should be too large for any empty tape");

        match err {
            ParityError::ObjectTooLargeForEmptyTape {
                projected_object_blocks,
                empty_tape_usable_blocks,
                required_reserve_blocks,
            } => {
                assert_eq!(projected_object_blocks, 20);
                assert_eq!(empty_tape_usable_blocks, 75);
                assert_eq!(required_reserve_blocks, 56);
            }
            other => panic!("expected empty-tape object-size error, got {other:?}"),
        }

        let current_tape_only = CapacityReserveInput {
            empty_tape_usable_blocks: 76,
            remaining_tape_blocks: 75,
            ..sample_input()
        };
        let err = current_tape_only
            .evaluate()
            .expect_err("object fits an empty tape but not the current one");
        match err {
            ParityError::CapacityReserveExceeded { cause, .. } => {
                assert_eq!(cause, CapacityReserveCause::TapeCapacity);
            }
            other => panic!("expected current-tape capacity error, got {other:?}"),
        }
    }

    #[test]
    fn spool_shortfall_reports_parity_spool_capacity() {
        let input = CapacityReserveInput {
            remaining_spool_bytes: 31 * 1024 - 1,
            ..sample_input()
        };
        let err = input.evaluate().expect_err("spool should be short");

        match err {
            ParityError::CapacityReserveExceeded {
                cause,
                projected_object_blocks,
                remaining_blocks,
                reserve_blocks,
                remaining_spool_bytes,
                required_spool_bytes,
            } => {
                assert_eq!(cause, CapacityReserveCause::ParitySpoolCapacity);
                assert_eq!(projected_object_blocks, 20);
                assert_eq!(remaining_blocks, None);
                assert_eq!(reserve_blocks, None);
                assert_eq!(remaining_spool_bytes, Some(31 * 1024 - 1));
                assert_eq!(required_spool_bytes, Some(31 * 1024));
            }
            other => panic!("expected capacity reserve error, got {other:?}"),
        }
    }

    #[test]
    fn spool_shortfall_separates_pending_and_new_sidecar_bytes() {
        let pending_only = CapacityReserveInput {
            projected_object_blocks: 1,
            block_size_bytes: 1024,
            current_epoch_fill_blocks: 0,
            data_shards_per_epoch: 12,
            parity_shards_per_epoch: 6,
            sidecar_index_block_count: 2,
            object_filemark_blocks: 1,
            sidecar_filemark_blocks: 1,
            bootstrap_filemark_blocks: 1,
            pending_completed_sidecars: 0,
            remaining_bootstrap_count: 0,
            safety_margin_blocks: 0,
            remaining_tape_blocks: u64::MAX,
            empty_tape_usable_blocks: u64::MAX,
            pending_completed_epoch_parity_bytes: 4096,
            remaining_spool_bytes: 4095,
        };
        assert_spool_shortfall(pending_only, 4095, 4096);

        let completing_object = CapacityReserveInput {
            projected_object_blocks: 12,
            pending_completed_epoch_parity_bytes: 4096,
            remaining_spool_bytes: 4096 + (12 * 1024) - 1,
            ..pending_only
        };
        assert_spool_shortfall(
            completing_object,
            4096 + (12 * 1024) - 1,
            4096 + (12 * 1024),
        );
    }

    fn assert_spool_shortfall(
        input: CapacityReserveInput,
        expected_remaining_spool_bytes: u64,
        expected_required_spool_bytes: u64,
    ) {
        let err = input
            .evaluate()
            .expect_err("spool reserve should be the binding constraint");

        match err {
            ParityError::CapacityReserveExceeded {
                cause,
                projected_object_blocks,
                remaining_blocks,
                reserve_blocks,
                remaining_spool_bytes,
                required_spool_bytes,
            } => {
                assert_eq!(cause, CapacityReserveCause::ParitySpoolCapacity);
                assert_eq!(projected_object_blocks, input.projected_object_blocks);
                assert_eq!(remaining_blocks, None);
                assert_eq!(reserve_blocks, None);
                assert_eq!(remaining_spool_bytes, Some(expected_remaining_spool_bytes));
                assert_eq!(required_spool_bytes, Some(expected_required_spool_bytes));
            }
            other => panic!("expected parity-spool shortfall, got {other:?}"),
        }
    }

    #[test]
    fn capacity_reserve_causes_have_distinct_operator_remedies() {
        assert_eq!(
            CapacityReserveCause::TapeCapacity.remedy(),
            CapacityReserveRemedy::CloseTapeAndRetryOnAnotherTape
        );
        assert_eq!(
            CapacityReserveCause::ParitySpoolCapacity.remedy(),
            CapacityReserveRemedy::FreeOrIncreaseParitySpool
        );
    }

    #[test]
    fn sidecar_filemark_and_bootstrap_counts_are_load_bearing_tape_reserve_inputs() {
        let base = CapacityReserveInput {
            projected_object_blocks: 7,
            block_size_bytes: 1024,
            current_epoch_fill_blocks: 0,
            data_shards_per_epoch: 4,
            parity_shards_per_epoch: 2,
            sidecar_index_block_count: 1,
            object_filemark_blocks: 1,
            sidecar_filemark_blocks: 0,
            bootstrap_filemark_blocks: 1,
            pending_completed_sidecars: 1,
            remaining_bootstrap_count: 0,
            safety_margin_blocks: 0,
            remaining_tape_blocks: u64::MAX,
            empty_tape_usable_blocks: u64::MAX,
            pending_completed_epoch_parity_bytes: 0,
            remaining_spool_bytes: u64::MAX,
        };
        let base_report = base.evaluate().expect("base reserve fits");
        assert_eq!(base_report.epochs_completed_by_object, 1);
        assert!(base_report.final_partial_sidecar_needed);

        let sidecar_count_after_object = base.pending_completed_sidecars
            + base_report.epochs_completed_by_object
            + u64::from(base_report.final_partial_sidecar_needed);
        assert_eq!(
            sidecar_count_after_object, 3,
            "fixture should include pending, completed, and final-partial sidecars"
        );

        let sidecar_filemark_blocks = 2;
        let with_sidecar_filemarks = CapacityReserveInput {
            sidecar_filemark_blocks,
            ..base
        };
        let sidecar_report = with_sidecar_filemarks
            .evaluate()
            .expect("sidecar-filemark reserve fits");
        assert_eq!(
            sidecar_report.reserve_after_object_blocks,
            base_report.reserve_after_object_blocks
                + sidecar_count_after_object * sidecar_filemark_blocks
        );

        let bootstrap_count = 2;
        let with_bootstraps = CapacityReserveInput {
            remaining_bootstrap_count: bootstrap_count,
            ..base
        };
        let bootstrap_report = with_bootstraps.evaluate().expect("bootstrap reserve fits");
        assert_eq!(
            bootstrap_report.reserve_after_object_blocks,
            base_report.reserve_after_object_blocks
                + bootstrap_count * bootstrap_report.bootstrap_tape_file_blocks
        );

        let short_on_sidecar_filemarks = CapacityReserveInput {
            remaining_tape_blocks: sidecar_report.required_tape_blocks - 1,
            ..with_sidecar_filemarks
        };
        match short_on_sidecar_filemarks
            .evaluate()
            .expect_err("sidecar filemark blocks must affect the tape-capacity gate")
        {
            ParityError::CapacityReserveExceeded {
                cause,
                remaining_blocks,
                reserve_blocks,
                remaining_spool_bytes,
                required_spool_bytes,
                ..
            } => {
                assert_eq!(cause, CapacityReserveCause::TapeCapacity);
                assert_eq!(
                    remaining_blocks,
                    Some(sidecar_report.required_tape_blocks - 1)
                );
                assert_eq!(
                    reserve_blocks,
                    Some(sidecar_report.reserve_after_object_blocks)
                );
                assert_eq!(remaining_spool_bytes, None);
                assert_eq!(required_spool_bytes, None);
            }
            other => panic!("expected sidecar-filemark tape shortfall, got {other:?}"),
        }

        let short_on_bootstraps = CapacityReserveInput {
            remaining_tape_blocks: bootstrap_report.required_tape_blocks - 1,
            ..with_bootstraps
        };
        match short_on_bootstraps
            .evaluate()
            .expect_err("remaining bootstrap count must affect the tape-capacity gate")
        {
            ParityError::CapacityReserveExceeded {
                cause,
                remaining_blocks,
                reserve_blocks,
                remaining_spool_bytes,
                required_spool_bytes,
                ..
            } => {
                assert_eq!(cause, CapacityReserveCause::TapeCapacity);
                assert_eq!(
                    remaining_blocks,
                    Some(bootstrap_report.required_tape_blocks - 1)
                );
                assert_eq!(
                    reserve_blocks,
                    Some(bootstrap_report.reserve_after_object_blocks)
                );
                assert_eq!(remaining_spool_bytes, None);
                assert_eq!(required_spool_bytes, None);
            }
            other => panic!("expected bootstrap tape shortfall, got {other:?}"),
        }
    }

    #[test]
    fn huge_object_sidecar_cluster_spool_reserve_scales_without_overflow() {
        let projected_object_blocks = 12_000_000;
        let block_size_bytes = 512 * 1024;
        let data_shards_per_epoch = 12;
        let parity_shards_per_epoch = 6;
        let sidecar_index_block_count = 2;
        let sidecar_filemark_blocks = 1;
        let epochs_completed = projected_object_blocks / data_shards_per_epoch;
        let sidecar_tape_file_blocks =
            (2 * sidecar_index_block_count) + parity_shards_per_epoch + 1 + sidecar_filemark_blocks;
        let expected_spool_bytes = epochs_completed * sidecar_tape_file_blocks * block_size_bytes;

        let input = CapacityReserveInput {
            projected_object_blocks,
            block_size_bytes,
            current_epoch_fill_blocks: 0,
            data_shards_per_epoch,
            parity_shards_per_epoch,
            sidecar_index_block_count,
            object_filemark_blocks: 1,
            sidecar_filemark_blocks,
            bootstrap_filemark_blocks: 1,
            pending_completed_sidecars: 0,
            remaining_bootstrap_count: 1,
            safety_margin_blocks: 32,
            remaining_tape_blocks: u64::MAX,
            empty_tape_usable_blocks: u64::MAX,
            pending_completed_epoch_parity_bytes: 0,
            remaining_spool_bytes: expected_spool_bytes,
        };

        let report = input.evaluate().expect("huge object reserve fits");

        assert_eq!(report.epochs_completed_by_object, epochs_completed);
        assert!(!report.final_partial_sidecar_needed);
        assert_eq!(report.sidecar_tape_file_blocks, sidecar_tape_file_blocks);
        assert_eq!(report.required_spool_bytes, expected_spool_bytes);
        assert_eq!(
            report.reserve_after_object_blocks,
            1 + epochs_completed * sidecar_tape_file_blocks + 2 + 32
        );

        let short_spool = CapacityReserveInput {
            remaining_spool_bytes: expected_spool_bytes - 1,
            ..input
        };
        let err = short_spool
            .evaluate()
            .expect_err("huge sidecar cluster must fail on spool capacity");

        match err {
            ParityError::CapacityReserveExceeded {
                cause,
                remaining_blocks,
                reserve_blocks,
                remaining_spool_bytes,
                required_spool_bytes,
                ..
            } => {
                assert_eq!(cause, CapacityReserveCause::ParitySpoolCapacity);
                assert_eq!(remaining_blocks, None);
                assert_eq!(reserve_blocks, None);
                assert_eq!(remaining_spool_bytes, Some(expected_spool_bytes - 1));
                assert_eq!(required_spool_bytes, Some(expected_spool_bytes));
            }
            other => panic!("expected parity-spool shortfall, got {other:?}"),
        }
    }

    #[test]
    fn exact_epoch_boundary_does_not_reserve_final_partial_sidecar() {
        let input = CapacityReserveInput {
            projected_object_blocks: 19,
            remaining_tape_blocks: 63,
            remaining_spool_bytes: 31 * 1024,
            ..sample_input()
        };
        let report = input.evaluate().expect("reserve fits");

        assert_eq!(report.epochs_completed_by_object, 2);
        assert!(!report.final_partial_sidecar_needed);
        assert_eq!(report.reserve_after_object_blocks, 44);
        assert_eq!(report.required_tape_blocks, 63);
    }

    #[test]
    fn rejects_epoch_fill_outside_open_epoch() {
        let input = CapacityReserveInput {
            current_epoch_fill_blocks: 12,
            ..sample_input()
        };
        let err = input.evaluate().expect_err("open epoch fill is invalid");

        match err {
            ParityError::Invariant(msg) => assert!(msg.contains("current epoch fill"), "{msg}"),
            other => panic!("expected invariant error, got {other:?}"),
        }
    }
}
