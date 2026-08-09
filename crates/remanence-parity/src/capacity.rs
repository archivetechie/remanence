//! Exact terminal-triple capacity authority for Layer 3c.
//!
//! Every Object admission and manual terminal close is derived from one checked
//! projection of the live parity prefix, physical capacity basis, pool
//! watermarks, terminal replicas, separation extents, safety allowance, and
//! atomic spool availability.  An Object writer receives an opaque
//! [`TerminalTripleObjectReservation`], so it cannot substitute an unchecked
//! report or a different geometry after admission.

use crate::error::ParityError;
use crate::index_separation::{index_separation_records, DEFAULT_INDEX_SEPARATION_BYTES};
use crate::parity_map::{
    parity_map_directory_len_upper_bound, parity_map_payload_len_upper_bound, PARITY_MAP_HEADER_LEN,
};
use crate::replicated_control::checked_replicated_control_layout;
use crate::sidecar::checked_sidecar_index_capacity_layout;
use crate::tape_index_replica::{checked_tape_index_replica_layout, TapeIndexReplicaCounts};

/// Reason a Layer 3c object-start capacity reservation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityReserveCause {
    /// Not enough remaining tape capacity for the projected object, its
    /// trailing filemark, pending sidecars, final sidecar, external ParityMap,
    /// three terminal replicas, two separation extents, and safety margin.
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

/// Inputs to the exact terminal-triple close calculation.
///
/// The same kernel supports ordinary Object admission and operator close-out.
/// `projected_object_present=false` evaluates the current reconciled prefix
/// without inventing an Object row or filemark; it is the capacity authority
/// for manual finalization below the automatic low watermark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalTripleCloseInput {
    /// Whether this evaluation projects one new complete Object.
    pub projected_object_present: bool,
    /// Conservative body-block charge for the proposed Object.
    pub projected_object_blocks: u64,
    /// Fixed tape-record size accepted by the terminal-index codec.
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
    /// Conservative capacity charge of one replica trailing filemark.
    pub replica_filemark_blocks: u64,
    /// Conservative capacity charge of one gap trailing filemark.
    pub gap_filemark_blocks: u64,
    /// Nominal total bytes in each typed separation extent, including frames.
    pub gap_nominal_bytes: u64,
    /// Additional closeout blocks held back by policy.
    pub safety_margin_blocks: u64,
    /// Blocks remaining from the current physical cursor to capacity basis C.
    pub remaining_tape_blocks: u64,
    /// Conservative cartridge/profile capacity basis `C` in fixed blocks.
    pub capacity_basis_blocks: u64,
    /// Pool low watermark `L` under the same capacity basis.
    pub low_watermark_blocks: u64,
    /// Maximum ordinary committed boundary `H`; the worst legal close must fit
    /// in the checked band `C - H` before the first Object is admitted.
    pub high_watermark_blocks: u64,
    /// Bytes already reserved in the parity spool.
    pub pending_completed_epoch_parity_bytes: u64,
    /// Bytes still available in the parity spool.
    pub remaining_spool_bytes: u64,
}

/// Every nonoverlapping component of one successful projected prefix and its
/// exact terminal-triple close bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalTripleCloseReport {
    /// Whether this evaluation included one proposed Object.
    pub projected_object_present: bool,
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
    /// Proposed Object body and trailing filemark, or zero for close-only.
    pub object_tape_file_blocks: u64,
    /// Proposed Object plus completed-sidecar charge before terminal close.
    pub prefix_commit_charge_blocks: u64,
    /// Object recovery rows after the optional proposed Object.
    pub object_rows_after: u64,
    /// Sidecar-directory rows after terminal partial-sidecar emission.
    pub sidecar_entries_after_closeout: u64,
    /// Capacity-derived upper bound on physically possible sidecar rows.
    pub maximum_sidecar_entries_for_capacity: u64,
    /// Complete pre-A structural map row count.
    pub structural_entries_after_closeout: u64,
    /// Exact charge for the terminal partial sidecar's projected CRC-index
    /// rows, fixed parity shards, footer, and filemark, or zero.
    pub final_partial_sidecar_blocks: u64,
    /// Whether closeout emits the required external ParityMap. Every non-empty
    /// sidecar directory has exactly one external final ParityMap.
    pub final_parity_map_needed: bool,
    /// Allocation-free canonical directory byte bound.
    pub final_parity_map_directory_bound_bytes: u64,
    /// Allocation-free complete ParityMap payload byte bound.
    pub final_parity_map_payload_bound_bytes: u64,
    /// Replicated ParityMap blocks excluding filemark, or zero.
    pub final_parity_map_blocks_before_filemark: u64,
    /// Replicated ParityMap blocks including filemark, or zero.
    pub final_parity_map_tape_file_blocks: u64,
    /// Exact fixed-slot bytes in each complete terminal replica payload.
    pub replica_payload_bytes: u64,
    /// Exact payload-only records in each replica.
    pub replica_payload_record_count: u64,
    /// Header, payload records, and local footer before the filemark.
    pub replica_records_before_filemark: u64,
    /// One complete replica plus its conservative filemark charge.
    pub replica_tape_file_blocks: u64,
    /// Charge for exactly three complete replicas.
    pub triple_replica_blocks: u64,
    /// Nominal total bytes in each typed separation extent.
    pub gap_nominal_bytes: u64,
    /// Exact records in each gap, including its header and footer.
    pub gap_records_before_filemark: u64,
    /// One complete gap plus its conservative filemark charge.
    pub gap_tape_file_blocks: u64,
    /// Charge for exactly two typed gaps.
    pub double_gap_blocks: u64,
    /// Final partial sidecar and external ParityMap, each charged once.
    pub parity_closeout_charge_blocks: u64,
    /// Three replicas and two gaps, excluding parity closeout and safety.
    pub terminal_tail_charge_blocks: u64,
    /// Caller policy's explicit safety allowance.
    pub safety_margin_blocks: u64,
    /// Parity closeout, complete five-file terminal tail, and safety.
    pub close_bound_blocks: u64,
    /// Projected prefix commit charge plus the complete close bound.
    pub required_tape_blocks: u64,
    /// Local bytes required for parity completed by this Object.
    pub required_spool_bytes: u64,
}

/// Checked Object-start authority consumed by the parity sink.
///
/// The fields are deliberately private.  The only constructor runs the exact
/// terminal-triple calculator, including tape and spool gates, and the sink
/// cross-checks the embedded live-state projection before beginning motion.
#[derive(Debug)]
pub struct TerminalTripleObjectReservation {
    input: TerminalTripleCloseInput,
    report: TerminalTripleCloseReport,
}

impl TerminalTripleObjectReservation {
    /// Exact terminal-triple report proved for this Object.
    pub fn report(&self) -> &TerminalTripleCloseReport {
        &self.report
    }

    /// Projected stored-representation body blocks bound by this reservation.
    pub fn projected_object_blocks(&self) -> u64 {
        self.input.projected_object_blocks
    }

    /// Fixed block size bound by this reservation.
    pub fn block_size_bytes(&self) -> u32 {
        self.input.block_size_bytes
    }

    pub(crate) fn input(&self) -> &TerminalTripleCloseInput {
        &self.input
    }

    pub(crate) fn into_parts(self) -> (TerminalTripleCloseInput, TerminalTripleCloseReport) {
        (self.input, self.report)
    }
}

/// Unit-test adapter for exercising legacy sink fixtures with tiny in-memory
/// blocks. The report is calculated with the smallest supported terminal block
/// size and, when the fixture predates Bootstrap accounting, one conservative
/// BOT structural row. The embedded input is restored to the fixture's actual
/// state so the production sink cross-check remains exact.
#[cfg(test)]
pub(crate) fn reserve_terminal_object_for_sink_test(
    mut input: TerminalTripleCloseInput,
) -> Result<TerminalTripleObjectReservation, ParityError> {
    let sink_block_size = input.block_size_bytes;
    let sink_structural_entries = input.structural_entries_before_object;
    if input.structural_entries_before_object == 0 {
        input.structural_entries_before_object = 1;
    }
    input.block_size_bytes = 256 * 1024;
    let report = input.evaluate()?;
    input.block_size_bytes = sink_block_size;
    input.structural_entries_before_object = sink_structural_entries;
    Ok(TerminalTripleObjectReservation { input, report })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParityMapCapacityLayout {
    payload_bound_bytes: u64,
    blocks_before_filemark: u64,
    tape_file_blocks: u64,
}

fn checked_parity_map_capacity_layout(
    block_size_bytes: u64,
    sidecar_entry_count: u64,
    filemark_blocks: u64,
) -> Result<ParityMapCapacityLayout, ParityError> {
    let payload_bound_bytes = parity_map_payload_len_upper_bound(sidecar_entry_count)?;
    let blocks_before_filemark = checked_replicated_control_layout(
        block_size_bytes,
        u64::try_from(PARITY_MAP_HEADER_LEN)
            .map_err(|_| ParityError::Invariant("parity-map header length overflows u64"))?,
        payload_bound_bytes,
        "parity-map capacity bound",
    )?
    .total_block_count;
    let tape_file_blocks = checked_add(blocks_before_filemark, filemark_blocks)?;
    Ok(ParityMapCapacityLayout {
        payload_bound_bytes,
        blocks_before_filemark,
        tape_file_blocks,
    })
}

impl TerminalTripleCloseInput {
    /// Check every exact-close gate and mint an opaque Object reservation.
    pub fn reserve_object(self) -> Result<TerminalTripleObjectReservation, ParityError> {
        if !self.projected_object_present {
            return Err(ParityError::Invariant(
                "terminal Object reservation requires a projected Object",
            ));
        }
        let report = self.evaluate()?;
        Ok(TerminalTripleObjectReservation {
            input: self,
            report,
        })
    }

    /// Evaluate the optional Object/prefix charge and exact terminal close.
    pub fn evaluate(self) -> Result<TerminalTripleCloseReport, ParityError> {
        if self.remaining_tape_blocks > self.capacity_basis_blocks {
            return Err(ParityError::Invariant(
                "terminal close remaining tape exceeds capacity basis C",
            ));
        }
        if self.low_watermark_blocks >= self.high_watermark_blocks
            || self.high_watermark_blocks > self.capacity_basis_blocks
        {
            return Err(ParityError::InvalidScheme(format!(
                "terminal close requires L < H <= C, got L={} H={} C={}",
                self.low_watermark_blocks, self.high_watermark_blocks, self.capacity_basis_blocks
            )));
        }
        checked_tape_index_replica_layout(
            self.block_size_bytes,
            TapeIndexReplicaCounts {
                structural_entry_count: 0,
                object_row_count: 0,
            },
        )
        .map_err(terminal_index_capacity_error)?;
        if self.projected_object_present != (self.projected_object_blocks != 0) {
            return Err(ParityError::Invariant(
                "terminal close projected Object presence and block charge disagree",
            ));
        }
        if self.gap_nominal_bytes != DEFAULT_INDEX_SEPARATION_BYTES {
            return Err(ParityError::InvalidScheme(format!(
                "terminal close separation extent is {} bytes, expected exactly {DEFAULT_INDEX_SEPARATION_BYTES}",
                self.gap_nominal_bytes
            )));
        }
        if self.data_shards_per_epoch == 0 {
            return Err(ParityError::InvalidScheme(
                "terminal close data_shards_per_epoch is zero".into(),
            ));
        }
        let parity_enabled = self.parity_shards_per_epoch != 0;
        if !parity_enabled
            && (self.current_epoch_fill_blocks != 0
                || self.pending_completed_sidecars != 0
                || self.sidecar_entries_before_object != 0
                || self.pending_completed_epoch_parity_bytes != 0)
        {
            return Err(ParityError::Invariant(
                "parity-off terminal close carries parity epoch, sidecar, or spool state",
            ));
        }
        let profile_neighborhood_blocks =
            checked_add(self.data_shards_per_epoch, self.parity_shards_per_epoch)?;
        if profile_neighborhood_blocks > u64::from(u32::MAX) {
            return Err(ParityError::InvalidScheme(format!(
                "terminal close profile neighborhood {profile_neighborhood_blocks} exceeds u32::MAX"
            )));
        }
        if self.current_epoch_fill_blocks >= self.data_shards_per_epoch {
            return Err(ParityError::Invariant(
                "terminal close current epoch fill is outside the open epoch",
            ));
        }
        if self.object_rows_before_object > self.structural_entries_before_object {
            return Err(ParityError::Invariant(
                "terminal close Object rows exceed structural entries",
            ));
        }
        if self.sidecar_entries_before_object > self.structural_entries_before_object {
            return Err(ParityError::Invariant(
                "terminal close sidecar rows exceed structural entries",
            ));
        }
        let recovery_rows_before_object = checked_add(
            self.object_rows_before_object,
            self.sidecar_entries_before_object,
        )?;
        if recovery_rows_before_object > self.structural_entries_before_object {
            return Err(ParityError::Invariant(
                "terminal close Object and sidecar rows overlap structural entries",
            ));
        }
        // Every mapped structural file, including a bootstrap prefix, consumes
        // at least one physical block.  The capacity basis is therefore also
        // a hard ceiling on the committed structural-row count; there is no
        // hidden allowance outside this count.
        if self.structural_entries_before_object > self.capacity_basis_blocks {
            return Err(ParityError::Invariant(
                "terminal close committed structural entries exceed physical capacity bound",
            ));
        }
        if self.structural_entries_before_object == 0 {
            return Err(ParityError::Invariant(
                "terminal close prefix is missing the BOT Bootstrap row",
            ));
        }

        let sidecar_layout = parity_enabled
            .then(|| {
                checked_sidecar_index_capacity_layout(
                    u64::from(self.block_size_bytes),
                    self.parity_shards_per_epoch,
                    self.data_shards_per_epoch,
                )
            })
            .transpose()?;
        let sidecar_blocks_before_filemark = match sidecar_layout {
            Some(layout) => checked_add(
                checked_add(
                    checked_mul(2, layout.block_count)?,
                    self.parity_shards_per_epoch,
                )?,
                1, // footer locator
            )?,
            None => 0,
        };
        let sidecar_tape_file_blocks = if parity_enabled {
            checked_add(sidecar_blocks_before_filemark, self.sidecar_filemark_blocks)?
        } else {
            0
        };
        let maximum_sidecar_entries_for_capacity = self
            .validate_capacity_derived_profile_bounds(parity_enabled, sidecar_tape_file_blocks)
            .map_err(|error| {
                ParityError::InvalidScheme(format!(
                    "unsafe terminal close capacity profile: {error}"
                ))
            })?;

        let (
            epochs_completed_by_object,
            final_partial_sidecar_data_crc_entries,
            final_partial_sidecar_needed,
        ) = if parity_enabled {
            let projected_epoch_fill =
                checked_add(self.current_epoch_fill_blocks, self.projected_object_blocks)?;
            let remainder = projected_epoch_fill % self.data_shards_per_epoch;
            (
                projected_epoch_fill / self.data_shards_per_epoch,
                remainder,
                remainder != 0,
            )
        } else {
            (0, 0, false)
        };
        let sidecars_emitted_by_commit =
            checked_add(self.pending_completed_sidecars, epochs_completed_by_object)?;
        let sidecar_blocks_emitted_by_commit =
            checked_mul(sidecars_emitted_by_commit, sidecar_tape_file_blocks)?;
        let object_tape_file_blocks = if self.projected_object_present {
            checked_add(self.projected_object_blocks, self.object_filemark_blocks)?
        } else {
            0
        };
        let prefix_commit_charge_blocks =
            checked_add(object_tape_file_blocks, sidecar_blocks_emitted_by_commit)?;

        let projected_object_count = u64::from(self.projected_object_present);
        let object_rows_after =
            checked_add(self.object_rows_before_object, projected_object_count)?;
        let sidecar_entries_after_commit = checked_add(
            self.sidecar_entries_before_object,
            sidecars_emitted_by_commit,
        )?;
        let final_partial_sidecar_count = u64::from(final_partial_sidecar_needed);
        let sidecar_entries_after_closeout =
            checked_add(sidecar_entries_after_commit, final_partial_sidecar_count)?;
        if self.sidecar_entries_before_object > maximum_sidecar_entries_for_capacity {
            return Err(ParityError::Invariant(
                "terminal close committed sidecar directory exceeds its physical capacity bound",
            ));
        }

        let final_parity_map_directory_bound_bytes =
            parity_map_directory_len_upper_bound(sidecar_entries_after_closeout)?;
        let final_parity_map_payload_bound_bytes =
            parity_map_payload_len_upper_bound(sidecar_entries_after_closeout)?;
        // The sole Bootstrap is fixed at BOT. A non-empty sidecar directory is
        // therefore represented by exactly one external final ParityMap.
        let final_parity_map_needed = sidecar_entries_after_closeout != 0;
        let final_parity_map_count = u64::from(final_parity_map_needed);
        let structural_entries_after_commit = checked_add(
            checked_add(
                self.structural_entries_before_object,
                projected_object_count,
            )?,
            sidecars_emitted_by_commit,
        )?;
        let structural_entries_after_closeout = checked_add(
            checked_add(structural_entries_after_commit, final_partial_sidecar_count)?,
            final_parity_map_count,
        )?;

        let final_parity_map_layout = if final_parity_map_needed {
            Some(checked_parity_map_capacity_layout(
                u64::from(self.block_size_bytes),
                sidecar_entries_after_closeout,
                self.parity_map_filemark_blocks,
            )?)
        } else {
            None
        };
        let final_parity_map_blocks_before_filemark = final_parity_map_layout
            .map(|layout| layout.blocks_before_filemark)
            .unwrap_or(0);
        let final_parity_map_tape_file_blocks = final_parity_map_layout
            .map(|layout| layout.tape_file_blocks)
            .unwrap_or(0);
        debug_assert_eq!(
            final_parity_map_layout.map(|layout| layout.payload_bound_bytes),
            final_parity_map_needed.then_some(final_parity_map_payload_bound_bytes)
        );
        let final_partial_sidecar_blocks = if final_partial_sidecar_needed {
            let layout = checked_sidecar_index_capacity_layout(
                u64::from(self.block_size_bytes),
                self.parity_shards_per_epoch,
                final_partial_sidecar_data_crc_entries,
            )?;
            checked_add(
                checked_add(
                    checked_add(
                        checked_mul(2, layout.block_count)?,
                        self.parity_shards_per_epoch,
                    )?,
                    1, // footer locator
                )?,
                self.sidecar_filemark_blocks,
            )?
        } else {
            0
        };
        let replica = checked_tape_index_replica_layout(
            self.block_size_bytes,
            TapeIndexReplicaCounts {
                structural_entry_count: structural_entries_after_closeout,
                object_row_count: object_rows_after,
            },
        )
        .map_err(terminal_index_capacity_error)?;
        let replica_tape_file_blocks =
            checked_add(replica.replica_record_count, self.replica_filemark_blocks)?;
        let triple_replica_blocks = checked_mul(3, replica_tape_file_blocks)?;
        let gap_records_before_filemark =
            index_separation_records(self.block_size_bytes, self.gap_nominal_bytes)
                .map_err(terminal_index_capacity_error)?;
        let gap_tape_file_blocks =
            checked_add(gap_records_before_filemark, self.gap_filemark_blocks)?;
        let double_gap_blocks = checked_mul(2, gap_tape_file_blocks)?;
        let parity_closeout_charge_blocks = checked_add(
            final_partial_sidecar_blocks,
            final_parity_map_tape_file_blocks,
        )?;
        let terminal_tail_charge_blocks = checked_add(triple_replica_blocks, double_gap_blocks)?;
        let close_bound_blocks = checked_sum(&[
            parity_closeout_charge_blocks,
            terminal_tail_charge_blocks,
            self.safety_margin_blocks,
        ])?;
        let required_tape_blocks = checked_add(prefix_commit_charge_blocks, close_bound_blocks)?;
        let required_reserve_blocks = required_tape_blocks
            .checked_sub(self.projected_object_blocks)
            .ok_or(ParityError::Invariant(
                "terminal close reserve subtraction underflow",
            ))?;

        if self.remaining_tape_blocks < required_tape_blocks {
            return Err(ParityError::CapacityReserveExceeded {
                cause: CapacityReserveCause::TapeCapacity,
                projected_object_blocks: self.projected_object_blocks,
                remaining_blocks: Some(self.remaining_tape_blocks),
                reserve_blocks: Some(required_reserve_blocks),
                remaining_spool_bytes: None,
                required_spool_bytes: None,
            });
        }

        let required_spool_bytes = if parity_enabled {
            let sidecar_bytes_before_filemark = checked_mul(
                sidecar_blocks_before_filemark,
                u64::from(self.block_size_bytes),
            )?;
            let newly_completed_sidecar_bytes =
                checked_mul(epochs_completed_by_object, sidecar_bytes_before_filemark)?;
            checked_add(
                self.pending_completed_epoch_parity_bytes,
                newly_completed_sidecar_bytes,
            )?
        } else {
            0
        };
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

        Ok(TerminalTripleCloseReport {
            projected_object_present: self.projected_object_present,
            epochs_completed_by_object,
            final_partial_sidecar_needed,
            sidecar_index_block_count: sidecar_layout.map_or(0, |layout| layout.block_count),
            sidecar_blocks_before_filemark,
            sidecar_tape_file_blocks,
            sidecars_emitted_by_commit,
            sidecar_blocks_emitted_by_commit,
            object_tape_file_blocks,
            prefix_commit_charge_blocks,
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
            replica_payload_bytes: replica.payload_len,
            replica_payload_record_count: replica.payload_record_count,
            replica_records_before_filemark: replica.replica_record_count,
            replica_tape_file_blocks,
            triple_replica_blocks,
            gap_nominal_bytes: self.gap_nominal_bytes,
            gap_records_before_filemark,
            gap_tape_file_blocks,
            double_gap_blocks,
            parity_closeout_charge_blocks,
            terminal_tail_charge_blocks,
            safety_margin_blocks: self.safety_margin_blocks,
            close_bound_blocks,
            required_tape_blocks,
            required_spool_bytes,
        })
    }

    /// Validate capacity-derived worst-case counts before the first Object can
    /// rely on this profile. This proves every bounded control-size function
    /// remains representable even if the media reaches its structural-count
    /// ceiling; it does not allocate those hypothetical rows.
    fn validate_capacity_derived_profile_bounds(
        self,
        parity_enabled: bool,
        maximum_complete_sidecar_tape_file_blocks: u64,
    ) -> Result<u64, ParityError> {
        let closeout_budget_blocks = self
            .capacity_basis_blocks
            .checked_sub(self.high_watermark_blocks)
            .ok_or_else(|| {
                ParityError::InvalidScheme(format!(
                    "high watermark {} exceeds capacity basis {}",
                    self.high_watermark_blocks, self.capacity_basis_blocks
                ))
            })?;
        if parity_enabled && maximum_complete_sidecar_tape_file_blocks > self.capacity_basis_blocks
        {
            return Err(ParityError::InvalidScheme(format!(
                "maximum complete sidecar requires {maximum_complete_sidecar_tape_file_blocks} blocks but capacity basis is {}",
                self.capacity_basis_blocks
            )));
        }
        let (maximum_sidecar_entries_for_capacity, maximum_parity_map_tape_file_blocks) =
            if parity_enabled {
                let minimum_sidecar_tape_file_blocks = checked_add(
                    checked_add(self.parity_shards_per_epoch, 3)?,
                    self.sidecar_filemark_blocks,
                )?;
                let maximum_sidecar_entries_for_capacity =
                    self.capacity_basis_blocks / minimum_sidecar_tape_file_blocks;
                let _maximum_directory_bytes =
                    parity_map_directory_len_upper_bound(maximum_sidecar_entries_for_capacity)?;
                let maximum_parity_map_payload_bytes =
                    parity_map_payload_len_upper_bound(maximum_sidecar_entries_for_capacity)?;
                let maximum_parity_map_blocks_before_filemark = checked_replicated_control_layout(
                    u64::from(self.block_size_bytes),
                    u64::try_from(PARITY_MAP_HEADER_LEN).map_err(|_| {
                        ParityError::Invariant("parity-map header length overflows u64")
                    })?,
                    maximum_parity_map_payload_bytes,
                    "maximum parity-map capacity bound",
                )?
                .total_block_count;
                let maximum_parity_map_tape_file_blocks =
                    if maximum_sidecar_entries_for_capacity == 0 {
                        0
                    } else {
                        checked_add(
                            maximum_parity_map_blocks_before_filemark,
                            self.parity_map_filemark_blocks,
                        )?
                    };
                (
                    maximum_sidecar_entries_for_capacity,
                    maximum_parity_map_tape_file_blocks,
                )
            } else {
                (0, 0)
            };

        // Every structural file and Object consumes at least one physical
        // block, so C is a conservative upper bound for either replica count.
        let maximum_replica = checked_tape_index_replica_layout(
            self.block_size_bytes,
            TapeIndexReplicaCounts {
                structural_entry_count: self.capacity_basis_blocks,
                object_row_count: self.capacity_basis_blocks,
            },
        )
        .map_err(terminal_index_capacity_error)?;
        let maximum_replica_tape_file_blocks = checked_add(
            maximum_replica.replica_record_count,
            self.replica_filemark_blocks,
        )?;
        let maximum_triple_replica_blocks = checked_mul(3, maximum_replica_tape_file_blocks)?;
        let gap_records = index_separation_records(self.block_size_bytes, self.gap_nominal_bytes)
            .map_err(terminal_index_capacity_error)?;
        let gap_tape_file_blocks = checked_add(gap_records, self.gap_filemark_blocks)?;
        let double_gap_blocks = checked_mul(2, gap_tape_file_blocks)?;
        let maximum_close_bound_blocks = checked_sum(&[
            maximum_complete_sidecar_tape_file_blocks,
            maximum_parity_map_tape_file_blocks,
            maximum_triple_replica_blocks,
            double_gap_blocks,
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

fn terminal_index_capacity_error(error: impl std::fmt::Display) -> ParityError {
    ParityError::InvalidScheme(format!(
        "terminal index capacity geometry is invalid: {error}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_terminal_close_input() -> TerminalTripleCloseInput {
        TerminalTripleCloseInput {
            projected_object_present: true,
            projected_object_blocks: 100,
            block_size_bytes: 256 * 1024,
            current_epoch_fill_blocks: 0,
            data_shards_per_epoch: 512 * 128,
            parity_shards_per_epoch: 512 * 4,
            pending_completed_sidecars: 0,
            sidecar_entries_before_object: 0,
            structural_entries_before_object: 1,
            object_rows_before_object: 0,
            object_filemark_blocks: 1,
            sidecar_filemark_blocks: 1,
            parity_map_filemark_blocks: 1,
            replica_filemark_blocks: 1,
            gap_filemark_blocks: 1,
            gap_nominal_bytes: 1 << 30,
            safety_margin_blocks: 4,
            remaining_tape_blocks: 10_367,
            capacity_basis_blocks: 10_371,
            low_watermark_blocks: 0,
            high_watermark_blocks: 65,
            pending_completed_epoch_parity_bytes: 0,
            remaining_spool_bytes: u64::MAX,
        }
    }

    #[test]
    fn terminal_close_reports_every_checked_nonoverlapping_component() {
        let report = sample_terminal_close_input()
            .evaluate()
            .expect("exact boundary must fit");

        assert!(report.projected_object_present);
        assert_eq!(report.epochs_completed_by_object, 0);
        assert!(report.final_partial_sidecar_needed);
        assert_eq!(report.sidecar_index_block_count, 3);
        assert_eq!(report.sidecar_blocks_before_filemark, 2_055);
        assert_eq!(report.sidecar_tape_file_blocks, 2_056);
        assert_eq!(report.sidecars_emitted_by_commit, 0);
        assert_eq!(report.object_tape_file_blocks, 101);
        assert_eq!(report.prefix_commit_charge_blocks, 101);
        assert_eq!(report.object_rows_after, 1);
        assert_eq!(report.sidecar_entries_after_closeout, 1);
        assert_eq!(report.structural_entries_after_closeout, 4);
        assert_eq!(report.final_partial_sidecar_blocks, 2_052);
        assert_eq!(
            report.sidecar_tape_file_blocks - report.final_partial_sidecar_blocks,
            4,
            "the 100-row partial CRC index is charged exactly, not as a full 65,536-row index"
        );
        assert!(report.final_parity_map_needed);
        assert_eq!(report.final_parity_map_directory_bound_bytes, 159);
        assert_eq!(report.final_parity_map_payload_bound_bytes, 441);
        assert_eq!(report.final_parity_map_blocks_before_filemark, 3);
        assert_eq!(report.final_parity_map_tape_file_blocks, 4);
        assert_eq!(report.replica_payload_bytes, 512);
        assert_eq!(report.replica_payload_record_count, 1);
        assert_eq!(report.replica_records_before_filemark, 3);
        assert_eq!(report.replica_tape_file_blocks, 4);
        assert_eq!(report.triple_replica_blocks, 12);
        assert_eq!(report.gap_nominal_bytes, 1 << 30);
        assert_eq!(report.gap_records_before_filemark, 4_096);
        assert_eq!(report.gap_tape_file_blocks, 4_097);
        assert_eq!(report.double_gap_blocks, 8_194);
        assert_eq!(report.parity_closeout_charge_blocks, 2_056);
        assert_eq!(report.terminal_tail_charge_blocks, 8_206);
        assert_eq!(report.close_bound_blocks, 10_266);
        assert_eq!(report.required_tape_blocks, 10_367);
        assert_eq!(report.required_spool_bytes, 0);
        assert_eq!(
            report.close_bound_blocks,
            report.parity_closeout_charge_blocks
                + report.triple_replica_blocks
                + report.double_gap_blocks
                + report.safety_margin_blocks
        );
    }

    #[test]
    fn terminal_close_capacity_gates_accept_equality_and_preserve_order() {
        let exact = sample_terminal_close_input();
        let exact_report = exact.evaluate().expect("C equality must succeed");
        assert_eq!(
            exact.remaining_tape_blocks, exact_report.required_tape_blocks,
            "the exact projected-remainder sidecar closes at equality"
        );

        assert_eq!(
            exact.capacity_basis_blocks - exact.high_watermark_blocks,
            10_306
        );
        exact
            .evaluate()
            .expect("worst-close equality with C-H must succeed");

        let impossible = TerminalTripleCloseInput {
            capacity_basis_blocks: exact.capacity_basis_blocks - 1,
            remaining_tape_blocks: exact.remaining_tape_blocks - 1,
            ..exact
        };
        assert!(matches!(
            impossible.evaluate(),
            Err(ParityError::InvalidScheme(message))
                if message.contains("C-H closeout budget")
        ));

        let current_short = TerminalTripleCloseInput {
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
    fn parity_off_terminal_close_reserves_only_triple_replicas_and_gaps() {
        let capacity = 20_000;
        let mut input = TerminalTripleCloseInput {
            projected_object_present: true,
            projected_object_blocks: 100,
            block_size_bytes: 256 * 1024,
            current_epoch_fill_blocks: 0,
            data_shards_per_epoch: 1,
            parity_shards_per_epoch: 0,
            pending_completed_sidecars: 0,
            sidecar_entries_before_object: 0,
            structural_entries_before_object: 1,
            object_rows_before_object: 0,
            object_filemark_blocks: 1,
            sidecar_filemark_blocks: 1,
            parity_map_filemark_blocks: 1,
            replica_filemark_blocks: 1,
            gap_filemark_blocks: 1,
            gap_nominal_bytes: DEFAULT_INDEX_SEPARATION_BYTES,
            safety_margin_blocks: 4,
            remaining_tape_blocks: capacity,
            capacity_basis_blocks: capacity,
            low_watermark_blocks: 0,
            high_watermark_blocks: 1,
            pending_completed_epoch_parity_bytes: 0,
            remaining_spool_bytes: 0,
        };

        let maximum_replica = checked_tape_index_replica_layout(
            input.block_size_bytes,
            TapeIndexReplicaCounts {
                structural_entry_count: capacity,
                object_row_count: capacity,
            },
        )
        .expect("capacity-derived maximum replica");
        let maximum_replica_tape_file_blocks = maximum_replica.replica_record_count + 1;
        let gap_tape_file_blocks =
            index_separation_records(input.block_size_bytes, input.gap_nominal_bytes)
                .expect("fixed separation geometry")
                + 1;
        let maximum_close_bound =
            3 * maximum_replica_tape_file_blocks + 2 * gap_tape_file_blocks + 4;
        input.high_watermark_blocks = capacity - maximum_close_bound;

        let report = input.evaluate().expect("parity-off equality must fit");
        assert_eq!(report.epochs_completed_by_object, 0);
        assert!(!report.final_partial_sidecar_needed);
        assert_eq!(report.sidecar_index_block_count, 0);
        assert_eq!(report.sidecar_tape_file_blocks, 0);
        assert_eq!(report.maximum_sidecar_entries_for_capacity, 0);
        assert!(!report.final_parity_map_needed);
        assert_eq!(report.parity_closeout_charge_blocks, 0);
        assert_eq!(report.required_spool_bytes, 0);
        assert_eq!(
            report.close_bound_blocks,
            report.triple_replica_blocks + report.double_gap_blocks + 4
        );

        let one_short = TerminalTripleCloseInput {
            high_watermark_blocks: input.high_watermark_blocks + 1,
            ..input
        };
        assert!(matches!(
            one_short.evaluate(),
            Err(ParityError::InvalidScheme(message))
                if message.contains("C-H closeout budget")
        ));
    }

    #[test]
    fn terminal_close_spool_excludes_filemarks_and_stays_distinct() {
        let projected_object_blocks = 512 * 128;
        let baseline = TerminalTripleCloseInput {
            projected_object_blocks,
            remaining_tape_blocks: 1_000_000,
            capacity_basis_blocks: 1_000_000,
            low_watermark_blocks: 0,
            high_watermark_blocks: 1,
            ..sample_terminal_close_input()
        };
        let report = baseline.evaluate().expect("full epoch fits");
        assert_eq!(report.epochs_completed_by_object, 1);
        assert!(!report.final_partial_sidecar_needed);
        assert_eq!(report.sidecars_emitted_by_commit, 1);
        assert_eq!(
            report.required_spool_bytes,
            report.sidecar_blocks_before_filemark * u64::from(baseline.block_size_bytes)
        );

        let short = TerminalTripleCloseInput {
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
    fn terminal_close_rejects_unbounded_or_inconsistent_counts() {
        let invalid_profile = TerminalTripleCloseInput {
            data_shards_per_epoch: u64::from(u32::MAX),
            parity_shards_per_epoch: 1,
            ..sample_terminal_close_input()
        };
        assert!(matches!(
            invalid_profile.evaluate(),
            Err(ParityError::InvalidScheme(_))
        ));

        let unbounded_capacity_profile = TerminalTripleCloseInput {
            capacity_basis_blocks: u64::MAX,
            remaining_tape_blocks: u64::MAX,
            low_watermark_blocks: 0,
            high_watermark_blocks: 1,
            ..sample_terminal_close_input()
        };
        assert!(matches!(
            unbounded_capacity_profile.evaluate(),
            Err(ParityError::InvalidScheme(_))
        ));

        let physically_unusable_profile = TerminalTripleCloseInput {
            data_shards_per_epoch: 2_000,
            parity_shards_per_epoch: 1_000,
            capacity_basis_blocks: 1_000,
            remaining_tape_blocks: 1_000,
            low_watermark_blocks: 0,
            high_watermark_blocks: 1,
            ..sample_terminal_close_input()
        };
        assert!(matches!(
            physically_unusable_profile.evaluate(),
            Err(ParityError::InvalidScheme(message))
                if message.contains("maximum complete sidecar")
        ));

        let physically_unusable_close_profile = TerminalTripleCloseInput {
            capacity_basis_blocks: 2_060,
            remaining_tape_blocks: 2_060,
            low_watermark_blocks: 0,
            high_watermark_blocks: 1,
            ..sample_terminal_close_input()
        };
        assert!(matches!(
            physically_unusable_close_profile.evaluate(),
            Err(ParityError::InvalidScheme(message))
                if message.contains("worst representable close")
        ));

        let closeout_band_too_narrow = TerminalTripleCloseInput {
            high_watermark_blocks: 66,
            ..sample_terminal_close_input()
        };
        assert!(matches!(
            closeout_band_too_narrow.evaluate(),
            Err(ParityError::InvalidScheme(message))
                if message.contains("C-H closeout budget")
        ));

        let invalid_high_watermark = TerminalTripleCloseInput {
            high_watermark_blocks: 10_372,
            ..sample_terminal_close_input()
        };
        assert!(matches!(
            invalid_high_watermark.evaluate(),
            Err(ParityError::InvalidScheme(message))
                if message.contains("L < H <= C")
        ));

        let invalid_rows = TerminalTripleCloseInput {
            object_rows_before_object: 2,
            structural_entries_before_object: 1,
            ..sample_terminal_close_input()
        };
        assert!(matches!(
            invalid_rows.evaluate(),
            Err(ParityError::Invariant(_))
        ));

        let overlapping_rows = TerminalTripleCloseInput {
            object_rows_before_object: 1,
            sidecar_entries_before_object: 1,
            structural_entries_before_object: 1,
            ..sample_terminal_close_input()
        };
        assert!(matches!(
            overlapping_rows.evaluate(),
            Err(ParityError::Invariant(_))
        ));

        let structurally_impossible_prefix = TerminalTripleCloseInput {
            structural_entries_before_object: 10_372,
            capacity_basis_blocks: 10_371,
            ..sample_terminal_close_input()
        };
        assert!(matches!(
            structurally_impossible_prefix.evaluate(),
            Err(ParityError::Invariant(
                "terminal close committed structural entries exceed physical capacity bound"
            ))
        ));

        let missing_bot = TerminalTripleCloseInput {
            structural_entries_before_object: 0,
            ..sample_terminal_close_input()
        };
        assert!(matches!(
            missing_bot.evaluate(),
            Err(ParityError::Invariant(
                "terminal close prefix is missing the BOT Bootstrap row"
            ))
        ));

        let too_many_new_sidecars = TerminalTripleCloseInput {
            projected_object_blocks: 512 * 128 * 3,
            capacity_basis_blocks: 10_371,
            remaining_tape_blocks: 10_371,
            ..sample_terminal_close_input()
        };
        assert!(matches!(
            too_many_new_sidecars.evaluate(),
            Err(ParityError::CapacityReserveExceeded {
                cause: CapacityReserveCause::TapeCapacity,
                ..
            })
        ));

        let overflow = TerminalTripleCloseInput {
            object_rows_before_object: u64::MAX,
            structural_entries_before_object: u64::MAX,
            ..sample_terminal_close_input()
        };
        assert!(matches!(
            overflow.evaluate(),
            Err(ParityError::Invariant(_))
        ));
    }

    #[test]
    fn manual_close_projects_no_object_below_the_automatic_watermarks() {
        let input = TerminalTripleCloseInput {
            projected_object_present: false,
            projected_object_blocks: 0,
            current_epoch_fill_blocks: 100,
            structural_entries_before_object: 2,
            object_rows_before_object: 1,
            remaining_tape_blocks: 999_900,
            capacity_basis_blocks: 1_000_000,
            low_watermark_blocks: 920_000,
            high_watermark_blocks: 980_000,
            ..sample_terminal_close_input()
        };

        let report = input
            .evaluate()
            .expect("early reconciled prefix must close");
        assert!(!report.projected_object_present);
        assert_eq!(report.object_tape_file_blocks, 0);
        assert_eq!(report.prefix_commit_charge_blocks, 0);
        assert_eq!(report.object_rows_after, input.object_rows_before_object);
        assert!(report.final_partial_sidecar_needed);
        assert_eq!(report.required_tape_blocks, report.close_bound_blocks);
        assert!(
            input.capacity_basis_blocks - input.remaining_tape_blocks < input.high_watermark_blocks,
            "fixture represents a tape far below the automatic close band"
        );
    }

    #[test]
    fn terminal_close_rejects_object_presence_charge_disagreement() {
        for input in [
            TerminalTripleCloseInput {
                projected_object_present: false,
                projected_object_blocks: 1,
                ..sample_terminal_close_input()
            },
            TerminalTripleCloseInput {
                projected_object_present: true,
                projected_object_blocks: 0,
                ..sample_terminal_close_input()
            },
        ] {
            assert!(matches!(input.evaluate(), Err(ParityError::Invariant(_))));
        }
    }

    #[test]
    fn object_reservation_is_minted_only_by_the_exact_checker() {
        let reservation = sample_terminal_close_input()
            .reserve_object()
            .expect("exact Object reservation");
        assert_eq!(reservation.projected_object_blocks(), 100);
        assert_eq!(reservation.block_size_bytes(), 256 * 1024);
        assert_eq!(reservation.report().required_tape_blocks, 10_367);

        let manual = TerminalTripleCloseInput {
            projected_object_present: false,
            projected_object_blocks: 0,
            ..sample_terminal_close_input()
        };
        assert!(matches!(
            manual.reserve_object(),
            Err(ParityError::Invariant(
                "terminal Object reservation requires a projected Object"
            ))
        ));
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
}
