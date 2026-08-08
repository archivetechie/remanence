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
use crate::parity_map::{
    parity_map_directory_len_upper_bound, parity_map_payload_len_upper_bound, PARITY_MAP_HEADER_LEN,
};
use crate::replicated_control::checked_replicated_control_layout;
use crate::sidecar::checked_sidecar_index_capacity_layout;
use crate::tape_index::{tape_index_snapshot_layout, TapeIndexSnapshotCounts};

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

/// Inputs to the proof-first snapshot-aware final-close calculation.
///
/// This base kernel projects an ordinary Object commit for which no checkpoint
/// or geometric policy control is due, followed by the mandatory final close.
/// A later scheduler must add every forced checkpoint/policy bundle to the
/// projected commit boundary before using the pool-admission kernel. This
/// candidate remains unwired until that scheduler and the accepted bootstrap
/// bytes exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotCloseInput {
    /// Conservative body-block charge for the proposed Object.
    pub projected_object_blocks: u64,
    /// Fixed tape-record size accepted by the snapshot codec.
    pub block_size_bytes: u32,
    /// Object-data shards already present in the live partial epoch.
    pub current_epoch_fill_blocks: u64,
    /// Maximum data CRC rows in one complete sidecar.
    pub data_shards_per_epoch: u64,
    /// Parity rows/blocks in one complete sidecar.
    pub parity_shards_per_epoch: u64,
    /// Completed sidecars staged before this Object begins.
    pub pending_completed_sidecars: u64,
    /// Committed sidecar-directory rows in the current prefix.
    pub sidecar_entries_before_object: u64,
    /// Committed structural map rows in the current prefix.
    pub structural_entries_before_object: u64,
    /// Committed Object recovery rows in the current prefix.
    pub object_rows_before_object: u64,
    /// Physical charge of one Object trailing filemark.
    pub object_filemark_blocks: u64,
    /// Physical charge of one sidecar trailing filemark.
    pub sidecar_filemark_blocks: u64,
    /// Physical charge of one ParityMap trailing filemark.
    pub parity_map_filemark_blocks: u64,
    /// Physical charge of one snapshot trailing filemark.
    pub snapshot_filemark_blocks: u64,
    /// Physical charge of the final bootstrap trailing filemark.
    pub bootstrap_filemark_blocks: u64,
    /// Additional closeout blocks held back by policy.
    pub safety_margin_blocks: u64,
    /// Blocks remaining from the current physical cursor to capacity basis C.
    pub remaining_tape_blocks: u64,
    /// Conservative usable blocks on fresh media under the same capacity basis.
    pub empty_tape_usable_blocks: u64,
    /// Maximum ordinary committed boundary `H`; the worst legal close must fit
    /// in the checked band `C - H` before the first Object is admitted.
    pub high_watermark_blocks: u64,
    /// Bytes already reserved in the parity spool.
    pub pending_completed_epoch_parity_bytes: u64,
    /// Bytes still available in the parity spool.
    pub remaining_spool_bytes: u64,
}

/// Every checked component of one successful ordinary Object commit and its
/// conservative final-close bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotCloseReport {
    /// Full parity epochs completed by the proposed Object.
    pub epochs_completed_by_object: u64,
    /// Whether finalization must emit one partial-epoch sidecar.
    pub final_partial_sidecar_needed: bool,
    /// Exact blocks in one sidecar header/index copy.
    pub sidecar_index_block_count: u64,
    /// Exact sidecar blocks excluding its trailing filemark.
    pub sidecar_blocks_before_filemark: u64,
    /// Exact sidecar blocks including its trailing filemark.
    pub sidecar_tape_file_blocks: u64,
    /// Pending and newly completed sidecars forced by the Object commit.
    pub sidecars_emitted_by_commit: u64,
    /// Physical charge for those commit-forced sidecars.
    pub sidecar_blocks_emitted_by_commit: u64,
    /// Proposed Object body and trailing filemark.
    pub object_tape_file_blocks: u64,
    /// Object and completed-sidecar charge for the no-policy-snapshot base
    /// kernel. A milestone scheduler must add its forced control bundle.
    pub object_commit_charge_blocks: u64,
    /// Object recovery rows after the proposed Object.
    pub object_rows_after: u64,
    /// Sidecar-directory rows after terminal partial-sidecar emission.
    pub sidecar_entries_after_closeout: u64,
    /// Capacity-derived upper bound on physically possible sidecar rows.
    pub maximum_sidecar_entries_for_capacity: u64,
    /// Prefix map rows before the new snapshot itself.
    pub structural_entries_after_closeout: u64,
    /// Maximum complete-layout charge reserved for a terminal partial sidecar,
    /// including filemark, or zero.
    pub final_partial_sidecar_blocks: u64,
    /// Whether the conservative close projection reserves an external
    /// ParityMap. Until the replacement bootstrap codec supplies a trusted
    /// inline-fit decision, every non-empty sidecar directory reserves one.
    pub final_parity_map_needed: bool,
    /// Allocation-free canonical directory byte bound.
    pub final_parity_map_directory_bound_bytes: u64,
    /// Allocation-free complete ParityMap payload byte bound.
    pub final_parity_map_payload_bound_bytes: u64,
    /// Replicated ParityMap blocks excluding filemark, or zero.
    pub final_parity_map_blocks_before_filemark: u64,
    /// Replicated ParityMap blocks including filemark, or zero.
    pub final_parity_map_tape_file_blocks: u64,
    /// Exact fixed-slot bytes in one snapshot payload copy.
    pub snapshot_payload_bytes: u64,
    /// Exact replicated snapshot blocks excluding filemark.
    pub snapshot_blocks_before_filemark: u64,
    /// Exact replicated snapshot blocks including filemark.
    pub snapshot_tape_file_blocks: u64,
    /// One-block final bootstrap plus its filemark.
    pub final_bootstrap_tape_file_blocks: u64,
    /// Caller policy's explicit safety allowance.
    pub safety_margin_blocks: u64,
    /// Conservative terminal partial-sidecar, ParityMap, snapshot, bootstrap,
    /// and safety bound.
    pub close_bound_blocks: u64,
    /// Base Object commit charge plus the conservative close bound.
    pub required_tape_blocks: u64,
    /// Local bytes required for parity completed by this Object.
    pub required_spool_bytes: u64,
}

impl SnapshotCloseInput {
    /// Evaluate the checked no-policy Object charge and conservative final
    /// close bound.
    pub fn evaluate(self) -> Result<SnapshotCloseReport, ParityError> {
        tape_index_snapshot_layout(
            self.block_size_bytes,
            TapeIndexSnapshotCounts {
                structural_entry_count: 0,
                object_row_count: 0,
            },
        )?;
        if self.data_shards_per_epoch == 0 {
            return Err(ParityError::InvalidScheme(
                "snapshot close data_shards_per_epoch is zero".into(),
            ));
        }
        if self.parity_shards_per_epoch == 0 {
            return Err(ParityError::InvalidScheme(
                "snapshot close parity_shards_per_epoch is zero".into(),
            ));
        }
        let profile_neighborhood_blocks =
            checked_add(self.data_shards_per_epoch, self.parity_shards_per_epoch)?;
        if profile_neighborhood_blocks > u64::from(u32::MAX) {
            return Err(ParityError::InvalidScheme(format!(
                "snapshot close profile neighborhood {profile_neighborhood_blocks} exceeds u32::MAX"
            )));
        }
        if self.current_epoch_fill_blocks >= self.data_shards_per_epoch {
            return Err(ParityError::Invariant(
                "snapshot close current epoch fill is outside the open epoch",
            ));
        }
        if self.object_rows_before_object > self.structural_entries_before_object {
            return Err(ParityError::Invariant(
                "snapshot close Object rows exceed structural entries",
            ));
        }
        if self.sidecar_entries_before_object > self.structural_entries_before_object {
            return Err(ParityError::Invariant(
                "snapshot close sidecar rows exceed structural entries",
            ));
        }
        let recovery_rows_before_object = checked_add(
            self.object_rows_before_object,
            self.sidecar_entries_before_object,
        )?;
        if recovery_rows_before_object > self.structural_entries_before_object {
            return Err(ParityError::Invariant(
                "snapshot close Object and sidecar rows overlap structural entries",
            ));
        }
        // Every mapped structural file, including a bootstrap prefix, consumes
        // at least one physical block.  The capacity basis is therefore also
        // a hard ceiling on the committed structural-row count; there is no
        // hidden allowance outside this count.
        if self.structural_entries_before_object > self.empty_tape_usable_blocks {
            return Err(ParityError::Invariant(
                "snapshot close committed structural entries exceed physical capacity bound",
            ));
        }

        let sidecar_layout = checked_sidecar_index_capacity_layout(
            u64::from(self.block_size_bytes),
            self.parity_shards_per_epoch,
            self.data_shards_per_epoch,
        )?;
        let replicated_sidecar_index_blocks = checked_mul(2, sidecar_layout.block_count)?;
        let sidecar_blocks_before_filemark = checked_add(
            checked_add(
                replicated_sidecar_index_blocks,
                self.parity_shards_per_epoch,
            )?,
            1, // footer locator
        )?;
        let sidecar_tape_file_blocks =
            checked_add(sidecar_blocks_before_filemark, self.sidecar_filemark_blocks)?;
        let maximum_sidecar_entries_for_capacity = self
            .validate_capacity_derived_profile_bounds(sidecar_tape_file_blocks)
            .map_err(|error| {
                ParityError::InvalidScheme(format!(
                    "unsafe snapshot close capacity profile: {error}"
                ))
            })?;

        let projected_epoch_fill =
            checked_add(self.current_epoch_fill_blocks, self.projected_object_blocks)?;
        let epochs_completed_by_object = projected_epoch_fill / self.data_shards_per_epoch;
        let final_partial_sidecar_needed = projected_epoch_fill % self.data_shards_per_epoch != 0;
        let sidecars_emitted_by_commit =
            checked_add(self.pending_completed_sidecars, epochs_completed_by_object)?;
        let sidecar_blocks_emitted_by_commit =
            checked_mul(sidecars_emitted_by_commit, sidecar_tape_file_blocks)?;
        let object_tape_file_blocks =
            checked_add(self.projected_object_blocks, self.object_filemark_blocks)?;
        let object_commit_charge_blocks =
            checked_add(object_tape_file_blocks, sidecar_blocks_emitted_by_commit)?;

        let object_rows_after = checked_add(self.object_rows_before_object, 1)?;
        let sidecar_entries_after_commit = checked_add(
            self.sidecar_entries_before_object,
            sidecars_emitted_by_commit,
        )?;
        let final_partial_sidecar_count = u64::from(final_partial_sidecar_needed);
        let sidecar_entries_after_closeout =
            checked_add(sidecar_entries_after_commit, final_partial_sidecar_count)?;
        if self.sidecar_entries_before_object > maximum_sidecar_entries_for_capacity {
            return Err(ParityError::Invariant(
                "snapshot close committed sidecar directory exceeds its physical capacity bound",
            ));
        }

        let final_parity_map_directory_bound_bytes =
            parity_map_directory_len_upper_bound(sidecar_entries_after_closeout)?;
        let final_parity_map_payload_bound_bytes =
            parity_map_payload_len_upper_bound(sidecar_entries_after_closeout)?;
        // The replacement bootstrap bytes are not accepted yet, so there is no
        // trusted codec-derived inline budget. Reserve the safe external form
        // for every non-empty directory; Stage 4 may refine this through the
        // accepted bootstrap codec, never through a caller-supplied byte count.
        let final_parity_map_needed = sidecar_entries_after_closeout != 0;
        let final_parity_map_count = u64::from(final_parity_map_needed);
        let structural_entries_after_commit = checked_add(
            checked_add(
                self.structural_entries_before_object,
                1, // proposed Object
            )?,
            sidecars_emitted_by_commit,
        )?;
        let structural_entries_after_closeout = checked_add(
            checked_add(structural_entries_after_commit, final_partial_sidecar_count)?,
            final_parity_map_count,
        )?;

        let snapshot = tape_index_snapshot_layout(
            self.block_size_bytes,
            TapeIndexSnapshotCounts {
                structural_entry_count: structural_entries_after_closeout,
                object_row_count: object_rows_after,
            },
        )?;
        let snapshot_tape_file_blocks =
            checked_add(snapshot.total_block_count, self.snapshot_filemark_blocks)?;

        let final_parity_map_blocks_before_filemark = if final_parity_map_needed {
            checked_replicated_control_layout(
                u64::from(self.block_size_bytes),
                u64::try_from(PARITY_MAP_HEADER_LEN).map_err(|_| {
                    ParityError::Invariant("parity-map header length overflows u64")
                })?,
                final_parity_map_payload_bound_bytes,
                "parity-map capacity bound",
            )?
            .total_block_count
        } else {
            0
        };
        let final_parity_map_tape_file_blocks = if final_parity_map_needed {
            checked_add(
                final_parity_map_blocks_before_filemark,
                self.parity_map_filemark_blocks,
            )?
        } else {
            0
        };
        let final_partial_sidecar_blocks = if final_partial_sidecar_needed {
            sidecar_tape_file_blocks
        } else {
            0
        };
        let final_bootstrap_tape_file_blocks = checked_add(
            self.block_count_per_bootstrap(),
            self.bootstrap_filemark_blocks,
        )?;
        let close_bound_blocks = checked_add(
            checked_add(
                checked_add(
                    final_partial_sidecar_blocks,
                    final_parity_map_tape_file_blocks,
                )?,
                snapshot_tape_file_blocks,
            )?,
            checked_add(final_bootstrap_tape_file_blocks, self.safety_margin_blocks)?,
        )?;
        let required_tape_blocks = checked_add(object_commit_charge_blocks, close_bound_blocks)?;

        if self.empty_tape_usable_blocks < required_tape_blocks {
            return Err(ParityError::ObjectTooLargeForEmptyTape {
                projected_object_blocks: self.projected_object_blocks,
                empty_tape_usable_blocks: self.empty_tape_usable_blocks,
                required_reserve_blocks: checked_add(
                    sidecar_blocks_emitted_by_commit,
                    close_bound_blocks,
                )?,
            });
        }
        if self.remaining_tape_blocks < required_tape_blocks {
            return Err(ParityError::CapacityReserveExceeded {
                cause: CapacityReserveCause::TapeCapacity,
                projected_object_blocks: self.projected_object_blocks,
                remaining_blocks: Some(self.remaining_tape_blocks),
                reserve_blocks: Some(checked_add(
                    sidecar_blocks_emitted_by_commit,
                    close_bound_blocks,
                )?),
                remaining_spool_bytes: None,
                required_spool_bytes: None,
            });
        }

        let sidecar_bytes_before_filemark = checked_mul(
            sidecar_blocks_before_filemark,
            u64::from(self.block_size_bytes),
        )?;
        let newly_completed_sidecar_bytes =
            checked_mul(epochs_completed_by_object, sidecar_bytes_before_filemark)?;
        let required_spool_bytes = checked_add(
            self.pending_completed_epoch_parity_bytes,
            newly_completed_sidecar_bytes,
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

        Ok(SnapshotCloseReport {
            epochs_completed_by_object,
            final_partial_sidecar_needed,
            sidecar_index_block_count: sidecar_layout.block_count,
            sidecar_blocks_before_filemark,
            sidecar_tape_file_blocks,
            sidecars_emitted_by_commit,
            sidecar_blocks_emitted_by_commit,
            object_tape_file_blocks,
            object_commit_charge_blocks,
            object_rows_after,
            sidecar_entries_after_closeout,
            maximum_sidecar_entries_for_capacity,
            structural_entries_after_closeout,
            final_partial_sidecar_blocks,
            final_parity_map_needed,
            final_parity_map_directory_bound_bytes,
            final_parity_map_payload_bound_bytes,
            final_parity_map_blocks_before_filemark,
            final_parity_map_tape_file_blocks,
            snapshot_payload_bytes: snapshot.payload_len,
            snapshot_blocks_before_filemark: snapshot.total_block_count,
            snapshot_tape_file_blocks,
            final_bootstrap_tape_file_blocks,
            safety_margin_blocks: self.safety_margin_blocks,
            close_bound_blocks,
            required_tape_blocks,
            required_spool_bytes,
        })
    }

    fn block_count_per_bootstrap(self) -> u64 {
        1
    }

    /// Validate capacity-derived worst-case counts before the first Object can
    /// rely on this profile. This proves every bounded control-size function
    /// remains representable even if the media reaches its structural-count
    /// ceiling; it does not allocate those hypothetical rows.
    fn validate_capacity_derived_profile_bounds(
        self,
        maximum_complete_sidecar_tape_file_blocks: u64,
    ) -> Result<u64, ParityError> {
        let closeout_budget_blocks = self
            .empty_tape_usable_blocks
            .checked_sub(self.high_watermark_blocks)
            .ok_or_else(|| {
                ParityError::InvalidScheme(format!(
                    "high watermark {} exceeds capacity basis {}",
                    self.high_watermark_blocks, self.empty_tape_usable_blocks
                ))
            })?;
        if maximum_complete_sidecar_tape_file_blocks > self.empty_tape_usable_blocks {
            return Err(ParityError::InvalidScheme(format!(
                "maximum complete sidecar requires {maximum_complete_sidecar_tape_file_blocks} blocks but capacity basis is {}",
                self.empty_tape_usable_blocks
            )));
        }
        let minimum_sidecar_tape_file_blocks = checked_add(
            checked_add(self.parity_shards_per_epoch, 3)?,
            self.sidecar_filemark_blocks,
        )?;
        let maximum_sidecar_entries_for_capacity =
            self.empty_tape_usable_blocks / minimum_sidecar_tape_file_blocks;
        let _maximum_directory_bytes =
            parity_map_directory_len_upper_bound(maximum_sidecar_entries_for_capacity)?;
        let maximum_parity_map_payload_bytes =
            parity_map_payload_len_upper_bound(maximum_sidecar_entries_for_capacity)?;
        let maximum_parity_map_blocks_before_filemark = checked_replicated_control_layout(
            u64::from(self.block_size_bytes),
            u64::try_from(PARITY_MAP_HEADER_LEN)
                .map_err(|_| ParityError::Invariant("parity-map header length overflows u64"))?,
            maximum_parity_map_payload_bytes,
            "maximum parity-map capacity bound",
        )?
        .total_block_count;
        let maximum_parity_map_tape_file_blocks = if maximum_sidecar_entries_for_capacity == 0 {
            0
        } else {
            checked_add(
                maximum_parity_map_blocks_before_filemark,
                self.parity_map_filemark_blocks,
            )?
        };

        // Every structural file and Object consumes at least one physical
        // block, so C is a conservative upper bound for either snapshot count.
        let maximum_snapshot = tape_index_snapshot_layout(
            self.block_size_bytes,
            TapeIndexSnapshotCounts {
                structural_entry_count: self.empty_tape_usable_blocks,
                object_row_count: self.empty_tape_usable_blocks,
            },
        )?;
        let maximum_snapshot_tape_file_blocks = checked_add(
            maximum_snapshot.total_block_count,
            self.snapshot_filemark_blocks,
        )?;
        let final_bootstrap_tape_file_blocks = checked_add(
            self.block_count_per_bootstrap(),
            self.bootstrap_filemark_blocks,
        )?;
        let maximum_close_bound_blocks = checked_sum(&[
            maximum_complete_sidecar_tape_file_blocks,
            maximum_parity_map_tape_file_blocks,
            maximum_snapshot_tape_file_blocks,
            final_bootstrap_tape_file_blocks,
            self.safety_margin_blocks,
        ])?;
        if maximum_close_bound_blocks > closeout_budget_blocks {
            return Err(ParityError::InvalidScheme(format!(
                "worst representable close requires {maximum_close_bound_blocks} blocks but C-H closeout budget is {closeout_budget_blocks}"
            )));
        }
        Ok(maximum_sidecar_entries_for_capacity)
    }
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

    fn sample_snapshot_close_input() -> SnapshotCloseInput {
        SnapshotCloseInput {
            projected_object_blocks: 100,
            block_size_bytes: 256 * 1024,
            current_epoch_fill_blocks: 0,
            data_shards_per_epoch: 512 * 128,
            parity_shards_per_epoch: 512 * 4,
            pending_completed_sidecars: 0,
            sidecar_entries_before_object: 0,
            structural_entries_before_object: 0,
            object_rows_before_object: 0,
            object_filemark_blocks: 1,
            sidecar_filemark_blocks: 1,
            parity_map_filemark_blocks: 1,
            snapshot_filemark_blocks: 1,
            bootstrap_filemark_blocks: 1,
            safety_margin_blocks: 4,
            remaining_tape_blocks: 2_171,
            empty_tape_usable_blocks: 2_171,
            high_watermark_blocks: 0,
            pending_completed_epoch_parity_bytes: 0,
            remaining_spool_bytes: u64::MAX,
        }
    }

    #[test]
    fn snapshot_close_reports_every_checked_close_bound_component() {
        let report = sample_snapshot_close_input()
            .evaluate()
            .expect("exact boundary must fit");

        assert_eq!(report.epochs_completed_by_object, 0);
        assert!(report.final_partial_sidecar_needed);
        assert_eq!(report.sidecar_index_block_count, 3);
        assert_eq!(report.sidecar_blocks_before_filemark, 2_055);
        assert_eq!(report.sidecar_tape_file_blocks, 2_056);
        assert_eq!(report.sidecars_emitted_by_commit, 0);
        assert_eq!(report.object_tape_file_blocks, 101);
        assert_eq!(report.object_commit_charge_blocks, 101);
        assert_eq!(report.object_rows_after, 1);
        assert_eq!(report.sidecar_entries_after_closeout, 1);
        assert_eq!(report.structural_entries_after_closeout, 3);
        assert_eq!(report.final_partial_sidecar_blocks, 2_056);
        assert!(report.final_parity_map_needed);
        assert_eq!(report.final_parity_map_directory_bound_bytes, 159);
        assert_eq!(report.final_parity_map_payload_bound_bytes, 441);
        assert_eq!(report.final_parity_map_blocks_before_filemark, 3);
        assert_eq!(report.final_parity_map_tape_file_blocks, 4);
        assert_eq!(report.snapshot_payload_bytes, 448);
        assert_eq!(report.snapshot_blocks_before_filemark, 3);
        assert_eq!(report.snapshot_tape_file_blocks, 4);
        assert_eq!(report.final_bootstrap_tape_file_blocks, 2);
        assert_eq!(report.close_bound_blocks, 2_070);
        assert_eq!(report.required_tape_blocks, 2_171);
        assert_eq!(report.required_spool_bytes, 0);
    }

    #[test]
    fn snapshot_close_capacity_gates_accept_equality_and_preserve_order() {
        let exact = sample_snapshot_close_input();
        exact.evaluate().expect("C equality must succeed");

        let closeout_band_equality = SnapshotCloseInput {
            high_watermark_blocks: 97,
            ..exact
        };
        closeout_band_equality
            .evaluate()
            .expect("worst-close equality with C-H must succeed");

        let impossible = SnapshotCloseInput {
            empty_tape_usable_blocks: exact.empty_tape_usable_blocks - 1,
            remaining_tape_blocks: exact.remaining_tape_blocks - 1,
            ..exact
        };
        assert!(matches!(
            impossible.evaluate(),
            Err(ParityError::ObjectTooLargeForEmptyTape { .. })
        ));

        let current_short = SnapshotCloseInput {
            remaining_tape_blocks: exact.remaining_tape_blocks - 1,
            ..exact
        };
        assert!(matches!(
            current_short.evaluate(),
            Err(ParityError::CapacityReserveExceeded {
                cause: CapacityReserveCause::TapeCapacity,
                ..
            })
        ));
    }

    #[test]
    fn snapshot_close_spool_excludes_filemarks_and_stays_distinct() {
        let projected_object_blocks = 512 * 128;
        let baseline = SnapshotCloseInput {
            projected_object_blocks,
            remaining_tape_blocks: 1_000_000,
            empty_tape_usable_blocks: 1_000_000,
            ..sample_snapshot_close_input()
        };
        let report = baseline.evaluate().expect("full epoch fits");
        assert_eq!(report.epochs_completed_by_object, 1);
        assert!(!report.final_partial_sidecar_needed);
        assert_eq!(report.sidecars_emitted_by_commit, 1);
        assert_eq!(
            report.required_spool_bytes,
            report.sidecar_blocks_before_filemark * u64::from(baseline.block_size_bytes)
        );

        let short = SnapshotCloseInput {
            remaining_spool_bytes: report.required_spool_bytes - 1,
            ..baseline
        };
        assert!(matches!(
            short.evaluate(),
            Err(ParityError::CapacityReserveExceeded {
                cause: CapacityReserveCause::ParitySpoolCapacity,
                ..
            })
        ));
    }

    #[test]
    fn snapshot_close_rejects_unbounded_or_inconsistent_counts() {
        let invalid_profile = SnapshotCloseInput {
            data_shards_per_epoch: u64::from(u32::MAX),
            parity_shards_per_epoch: 1,
            ..sample_snapshot_close_input()
        };
        assert!(matches!(
            invalid_profile.evaluate(),
            Err(ParityError::InvalidScheme(_))
        ));

        let unbounded_capacity_profile = SnapshotCloseInput {
            empty_tape_usable_blocks: u64::MAX,
            remaining_tape_blocks: u64::MAX,
            ..sample_snapshot_close_input()
        };
        assert!(matches!(
            unbounded_capacity_profile.evaluate(),
            Err(ParityError::InvalidScheme(_))
        ));

        let physically_unusable_profile = SnapshotCloseInput {
            data_shards_per_epoch: 2_000,
            parity_shards_per_epoch: 1_000,
            empty_tape_usable_blocks: 1_000,
            remaining_tape_blocks: 1_000,
            ..sample_snapshot_close_input()
        };
        assert!(matches!(
            physically_unusable_profile.evaluate(),
            Err(ParityError::InvalidScheme(message))
                if message.contains("maximum complete sidecar")
        ));

        let physically_unusable_close_profile = SnapshotCloseInput {
            empty_tape_usable_blocks: 2_060,
            remaining_tape_blocks: 2_060,
            ..sample_snapshot_close_input()
        };
        assert!(matches!(
            physically_unusable_close_profile.evaluate(),
            Err(ParityError::InvalidScheme(message))
                if message.contains("worst representable close")
        ));

        let closeout_band_too_narrow = SnapshotCloseInput {
            high_watermark_blocks: 100,
            ..sample_snapshot_close_input()
        };
        assert!(matches!(
            closeout_band_too_narrow.evaluate(),
            Err(ParityError::InvalidScheme(message))
                if message.contains("C-H closeout budget")
        ));

        let invalid_high_watermark = SnapshotCloseInput {
            high_watermark_blocks: 2_172,
            ..sample_snapshot_close_input()
        };
        assert!(matches!(
            invalid_high_watermark.evaluate(),
            Err(ParityError::InvalidScheme(message))
                if message.contains("high watermark")
        ));

        let invalid_rows = SnapshotCloseInput {
            object_rows_before_object: 2,
            structural_entries_before_object: 1,
            ..sample_snapshot_close_input()
        };
        assert!(matches!(
            invalid_rows.evaluate(),
            Err(ParityError::Invariant(_))
        ));

        let overlapping_rows = SnapshotCloseInput {
            object_rows_before_object: 1,
            sidecar_entries_before_object: 1,
            structural_entries_before_object: 1,
            ..sample_snapshot_close_input()
        };
        assert!(matches!(
            overlapping_rows.evaluate(),
            Err(ParityError::Invariant(_))
        ));

        let structurally_impossible_prefix = SnapshotCloseInput {
            structural_entries_before_object: 2_172,
            empty_tape_usable_blocks: 2_171,
            ..sample_snapshot_close_input()
        };
        assert!(matches!(
            structurally_impossible_prefix.evaluate(),
            Err(ParityError::Invariant(
                "snapshot close committed structural entries exceed physical capacity bound"
            ))
        ));

        let too_many_new_sidecars = SnapshotCloseInput {
            projected_object_blocks: 512 * 128 * 3,
            empty_tape_usable_blocks: 4_111,
            remaining_tape_blocks: 4_111,
            ..sample_snapshot_close_input()
        };
        assert!(matches!(
            too_many_new_sidecars.evaluate(),
            Err(ParityError::ObjectTooLargeForEmptyTape { .. })
        ));

        let overflow = SnapshotCloseInput {
            object_rows_before_object: u64::MAX,
            structural_entries_before_object: u64::MAX,
            ..sample_snapshot_close_input()
        };
        assert!(matches!(
            overflow.evaluate(),
            Err(ParityError::Invariant(_))
        ));
    }

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
