//! Object-scoped Layer 3c sidecar recovery source.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::Arc;

use remanence_library::scsi::{decode_sense, ScsiError};
use remanence_library::{BlockSource, SpaceKind, SpaceResult, TapeIoError, TapePosition};

use crate::error::ParityError;
use crate::filemark_map::{ScopedFilemarkMap, TapeFileKind, TapeFilePosition};
use crate::mapping::data_shards_per_epoch;
use crate::model::{
    ParityScheme, RecoveryEvent, RecoveryOutcome, SidecarMetadataHealth,
    SidecarMetadataHealthEvent, StripeAddress, StripePosition,
};
use crate::raw::{PhysicalPositionHint, RawReadOutcome, RawTapeSource};
use crate::recovery::{
    recover_object_block_from_sidecar, recover_object_region_from_sidecar, SidecarRecoveryResult,
};

const DEFAULT_MAX_RECOVERY_CACHE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const DEFAULT_MAX_STRIPES_PER_WINDOW: u32 = 1024;

/// Audit hook trait for recovery events. The daemon registers
/// an implementation that fans events into the same audit log
/// Layer 2's `LibraryAuditHook` feeds. `Send + Sync` because
/// the hook may be shared across the audit-emitting reader
/// thread and the daemon's log-consumer thread.
pub trait ParityAuditHook: Send + Sync {
    /// Called once per recovery attempt, after the attempt
    /// completes (success or failure). Implementations should
    /// be fast — the read path waits on this call.
    fn on_recovery(&self, event: &RecoveryEvent);

    /// Called when recovery observes that a sidecar remains usable only because
    /// one replicated metadata copy survived. The default is a no-op so older
    /// audit consumers continue to receive recovery events unchanged.
    fn on_sidecar_metadata_health(&self, _event: &SidecarMetadataHealthEvent) {}
}

/// Trust policy when opening an object from a scoped filemark map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenTrust {
    /// Reject objects that sit outside the authenticated prefix.
    RequireValidated,
    /// Allow clean tar-only reads from an unvalidated suffix object.
    ///
    /// Recovery remains disabled for this mode because the suffix is not
    /// authenticated by the selected bootstrap digest.
    AllowTarOnlyUnverified,
}

/// Memory-bound planning policy for bulk sidecar recovery.
///
/// This is the Layer 3c §9.3 epoch-planner budget. `recover_region` uses this
/// cap for its epoch-scoped peer cache and result buffer; isolated
/// `recover_block_at` calls remain the small single-block recovery path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BulkRecoveryPolicy {
    /// Hard cap for recovery cache bytes. This object-scoped Vec-returning
    /// surface also accounts for returned block bytes so it cannot hide heap
    /// growth behind the caller-owned result.
    pub max_recovery_cache_bytes: u64,
    /// Permit recovery to split an over-budget full-epoch plan into smaller
    /// planner windows instead of failing immediately.
    pub allow_windowed_recovery: bool,
    /// Upper bound on affected stripes recovered in one planner window.
    pub max_stripes_per_window: u32,
}

impl Default for BulkRecoveryPolicy {
    fn default() -> Self {
        Self {
            max_recovery_cache_bytes: DEFAULT_MAX_RECOVERY_CACHE_BYTES,
            allow_windowed_recovery: true,
            max_stripes_per_window: DEFAULT_MAX_STRIPES_PER_WINDOW,
        }
    }
}

/// Recovered object-local block region.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveredRegion {
    /// First object-local body LBA recovered.
    pub start_body_lba: u64,
    /// Recovered fixed-size object blocks, in ascending body-LBA order.
    pub blocks: Vec<Vec<u8>>,
}

/// One recovered block addressed in global `ParityDataOrdinal` space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveredOrdinalBlock {
    /// Global object-data ordinal recovered.
    pub ordinal: u64,
    /// Object tape file containing this ordinal.
    pub tape_file_number: u32,
    /// Object-local body LBA within `tape_file_number`.
    pub body_lba: u64,
    /// Recovered fixed-size object block.
    pub data: Vec<u8>,
}

/// Recovered contiguous ordinal range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveredOrdinalRange {
    /// First global `ParityDataOrdinal` requested.
    pub start_ordinal: u64,
    /// Recovered blocks in ascending ordinal order.
    pub blocks: Vec<RecoveredOrdinalBlock>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BulkRecoveryPlan {
    max_stripes_per_window: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BufferedAutoReadBlock {
    data: Vec<u8>,
    was_recovered: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum NextBodyLbaProbe {
    Erasure,
    CleanBlock { body_lba: u64, data: Vec<u8> },
    NoErasure,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AutoBulkProbeRun {
    block_count: u64,
    clean_tail: Option<(u64, Vec<u8>)>,
}

/// Object-local read surface for Layer 3c v0.4.4 sidecar parity.
///
/// Object tape files contain only body-format blocks, so clean reads are raw
/// passthrough within a single tape file. Recovery is object-scoped:
/// `recover_block_at(body_lba)` resolves through the authenticated filemark
/// map to a `ParityDataOrdinal` and delegates to the sidecar recovery core.
#[allow(missing_debug_implementations)]
pub struct ObjectParitySource<'a> {
    inner: &'a mut dyn RawTapeSource,
    scheme: ParityScheme,
    tape_uuid: [u8; 16],
    scoped_map: ScopedFilemarkMap,
    tape_file_number: u32,
    object_first_ordinal: u64,
    object_block_count: u64,
    block_size: u32,
    cursor_body_lba: u64,
    tar_only_unverified: bool,
    audit_hook: Option<Arc<dyn ParityAuditHook>>,
    auto_read_blocks: BTreeMap<u64, BufferedAutoReadBlock>,
    last_read_erasure_body_lba: Option<u64>,
    reported_sidecar_metadata_health: BTreeSet<(u32, u64, SidecarMetadataHealth)>,
}

impl<'a> ObjectParitySource<'a> {
    /// Open one object tape file as an object-local [`BlockSource`].
    ///
    /// The source is configured for the session fixed block size and positioned
    /// at body LBA 0. `OpenTrust::RequireValidated` rejects unvalidated-prefix
    /// suffix objects; `OpenTrust::AllowTarOnlyUnverified` permits clean reads
    /// from such objects while keeping recovery disabled.
    pub fn open(
        inner: &'a mut dyn RawTapeSource,
        scheme: ParityScheme,
        tape_uuid: [u8; 16],
        scoped_map: ScopedFilemarkMap,
        block_size: u32,
        tape_file_number: u32,
        trust: OpenTrust,
    ) -> Result<Self, ParityError> {
        scheme.validate()?;
        if block_size == 0 {
            return Err(ParityError::Invariant("object source block size is zero"));
        }

        let object = object_entry(&scoped_map, tape_file_number)?;
        let object_block_count = object.block_count;
        let object_first_ordinal = object.first_parity_data_ordinal.ok_or_else(|| {
            ParityError::FilemarkMapReconstruct(format!(
                "object tape file {tape_file_number} is missing first ordinal"
            ))
        })?;
        let tar_only_unverified = !scoped_map.is_validated(tape_file_number);
        if tar_only_unverified && trust == OpenTrust::RequireValidated {
            return Err(ParityError::OutsideValidatedMapPrefix {
                ordinal: object_first_ordinal,
                prefix_ordinals: validated_prefix_ordinals(&scoped_map),
            });
        }

        inner.configure_fixed_block_size(block_size)?;
        let mut source = Self {
            inner,
            scheme,
            tape_uuid,
            scoped_map,
            tape_file_number,
            object_first_ordinal,
            object_block_count,
            block_size,
            cursor_body_lba: 0,
            tar_only_unverified,
            audit_hook: None,
            auto_read_blocks: BTreeMap::new(),
            last_read_erasure_body_lba: None,
            reported_sidecar_metadata_health: BTreeSet::new(),
        };
        source.locate_body_lba(0)?;
        Ok(source)
    }

    /// Install an audit hook for sidecar recovery attempts.
    pub fn set_audit_hook(&mut self, hook: Option<Arc<dyn ParityAuditHook>>) {
        self.audit_hook = hook;
    }

    /// Whether this object was opened from an unvalidated suffix.
    pub fn is_tar_only_unverified(&self) -> bool {
        self.tar_only_unverified
    }

    /// Force recovery of one clean-reading-but-integrity-failed body block.
    pub fn recover_block_at(&mut self, body_lba: u64) -> Result<Vec<u8>, ParityError> {
        self.ensure_body_lba_in_range(body_lba)?;
        let fallback = self.recovery_fallback_event_parts(body_lba);
        match recover_object_block_from_sidecar(
            self.inner,
            &self.scoped_map,
            &self.scheme,
            self.tape_uuid,
            self.block_size,
            self.tape_file_number,
            body_lba,
        ) {
            Ok(result) => {
                self.cursor_body_lba = body_lba
                    .checked_add(1)
                    .ok_or(ParityError::Invariant("body LBA cursor overflows"))?;
                self.emit_sidecar_metadata_health_event(&result);
                self.emit_sidecar_recovery_event(&result, RecoveryOutcome::Recovered, body_lba);
                Ok(result.recovered_block)
            }
            Err(err) => {
                self.emit_failed_sidecar_recovery_event(fallback, &err, body_lba);
                Err(err)
            }
        }
    }

    /// Recover a contiguous object-local region through the sidecar path.
    ///
    /// This object-scoped surface still returns the recovered region as one
    /// `Vec`, so the policy accounts for both the returned block bytes and the
    /// epoch/window recovery cache budget.
    pub fn recover_region(
        &mut self,
        start_body_lba: u64,
        block_count: u64,
        policy: BulkRecoveryPolicy,
    ) -> Result<RecoveredRegion, ParityError> {
        self.recover_region_with_failure_audit(start_body_lba, block_count, policy, true)
    }

    /// Recover a contiguous global `ParityDataOrdinal` range through this
    /// object source.
    ///
    /// The current source is object-scoped, so the requested range must map
    /// entirely to this object's tape file. The method translates ordinals to
    /// object-local body LBAs and then reuses the epoch-scoped bulk planner
    /// used by [`Self::recover_region`].
    pub fn recover_ordinal_range(
        &mut self,
        ordinals: Range<u64>,
        policy: BulkRecoveryPolicy,
    ) -> Result<RecoveredOrdinalRange, ParityError> {
        let start_ordinal = ordinals.start;
        let (start_body_lba, block_count) = self.ordinal_range_to_body_region(ordinals)?;
        let recovered = self.recover_region(start_body_lba, block_count, policy)?;
        let mut blocks = Vec::with_capacity(recovered.blocks.len());
        for (index, data) in recovered.blocks.into_iter().enumerate() {
            let offset = u64::try_from(index)
                .map_err(|_| ParityError::Invariant("ordinal recovery index overflows"))?;
            let ordinal = start_ordinal
                .checked_add(offset)
                .ok_or(ParityError::Invariant("recovered ordinal overflows"))?;
            let body_lba = recovered
                .start_body_lba
                .checked_add(offset)
                .ok_or(ParityError::Invariant("recovered body LBA overflows"))?;
            blocks.push(RecoveredOrdinalBlock {
                ordinal,
                tape_file_number: self.tape_file_number,
                body_lba,
                data,
            });
        }
        Ok(RecoveredOrdinalRange {
            start_ordinal,
            blocks,
        })
    }

    fn recover_region_with_failure_audit(
        &mut self,
        start_body_lba: u64,
        block_count: u64,
        policy: BulkRecoveryPolicy,
        emit_failure_audit: bool,
    ) -> Result<RecoveredRegion, ParityError> {
        if policy.max_recovery_cache_bytes == 0 {
            return Err(ParityError::Invariant(
                "bulk recovery max_recovery_cache_bytes is zero",
            ));
        }
        if policy.max_stripes_per_window == 0 {
            return Err(ParityError::Invariant(
                "bulk recovery max_stripes_per_window is zero",
            ));
        }
        let end_body_lba = start_body_lba
            .checked_add(block_count)
            .ok_or(ParityError::Invariant("bulk recovery range overflows"))?;
        if block_count == 0 {
            if start_body_lba > self.object_block_count {
                return Err(ParityError::FilemarkMapReconstruct(format!(
                    "empty bulk recovery start {start_body_lba} exceeds object block_count {}",
                    self.object_block_count
                )));
            }
            return Ok(RecoveredRegion {
                start_body_lba,
                blocks: Vec::new(),
            });
        }
        if end_body_lba > self.object_block_count {
            return Err(ParityError::FilemarkMapReconstruct(format!(
                "bulk recovery range [{start_body_lba}, {end_body_lba}) exceeds object block_count {}",
                self.object_block_count
            )));
        }
        let plan =
            self.plan_bulk_recovery_budget(start_body_lba, end_body_lba, block_count, policy)?;

        let recovered = match recover_object_region_from_sidecar(
            self.inner,
            &self.scoped_map,
            &self.scheme,
            self.tape_uuid,
            self.block_size,
            self.tape_file_number,
            start_body_lba,
            block_count,
            plan.max_stripes_per_window,
        ) {
            Ok(recovered) => recovered,
            Err(err) => {
                if emit_failure_audit {
                    let fallback = self.recovery_fallback_event_parts(start_body_lba);
                    self.emit_failed_sidecar_recovery_event(fallback, &err, start_body_lba);
                }
                return Err(err);
            }
        };
        let mut blocks = Vec::with_capacity(recovered.len());
        for block in recovered {
            self.emit_sidecar_metadata_health_event(&block.result);
            self.emit_sidecar_recovery_event(
                &block.result,
                RecoveryOutcome::Recovered,
                block.body_lba,
            );
            blocks.push(block.result.recovered_block);
        }
        self.locate_body_lba(end_body_lba)?;
        Ok(RecoveredRegion {
            start_body_lba,
            blocks,
        })
    }

    fn plan_bulk_recovery_budget(
        &self,
        start_body_lba: u64,
        end_body_lba: u64,
        block_count: u64,
        policy: BulkRecoveryPolicy,
    ) -> Result<BulkRecoveryPlan, ParityError> {
        let output_bytes = checked_mul(
            block_count,
            u64::from(self.block_size),
            "bulk recovery output bytes",
        )?;
        if output_bytes > policy.max_recovery_cache_bytes {
            return Err(ParityError::RecoveryPlanExceedsMemoryBudget {
                needed_bytes: output_bytes,
                max_recovery_cache_bytes: policy.max_recovery_cache_bytes,
                allow_windowed_recovery: policy.allow_windowed_recovery,
            });
        }
        let affected_stripes = self.affected_stripe_count(start_body_lba, end_body_lba)?;
        let stripe_cache_bytes = self.stripe_recovery_cache_bytes()?;
        let full_plan_bytes = checked_add(
            output_bytes,
            checked_mul(
                affected_stripes,
                stripe_cache_bytes,
                "bulk recovery full-plan cache bytes",
            )?,
            "bulk recovery full-plan bytes",
        )?;
        if full_plan_bytes <= policy.max_recovery_cache_bytes {
            return Ok(BulkRecoveryPlan {
                max_stripes_per_window: affected_stripes.max(1),
            });
        }
        if !policy.allow_windowed_recovery {
            return Err(ParityError::RecoveryPlanExceedsMemoryBudget {
                needed_bytes: full_plan_bytes,
                max_recovery_cache_bytes: policy.max_recovery_cache_bytes,
                allow_windowed_recovery: false,
            });
        }

        let available_cache_bytes = policy.max_recovery_cache_bytes.saturating_sub(output_bytes);
        let budget_stripes = available_cache_bytes / stripe_cache_bytes;
        if budget_stripes == 0 {
            let needed_bytes = checked_add(
                output_bytes,
                stripe_cache_bytes,
                "bulk recovery minimum window bytes",
            )?;
            return Err(ParityError::RecoveryPlanExceedsMemoryBudget {
                needed_bytes,
                max_recovery_cache_bytes: policy.max_recovery_cache_bytes,
                allow_windowed_recovery: true,
            });
        }

        let window_stripes = affected_stripes
            .min(u64::from(policy.max_stripes_per_window))
            .min(budget_stripes)
            .max(1);
        let window_plan_bytes = checked_add(
            output_bytes,
            checked_mul(
                window_stripes,
                stripe_cache_bytes,
                "bulk recovery window cache bytes",
            )?,
            "bulk recovery window bytes",
        )?;
        if window_plan_bytes > policy.max_recovery_cache_bytes {
            return Err(ParityError::RecoveryPlanExceedsMemoryBudget {
                needed_bytes: window_plan_bytes,
                max_recovery_cache_bytes: policy.max_recovery_cache_bytes,
                allow_windowed_recovery: true,
            });
        }
        Ok(BulkRecoveryPlan {
            max_stripes_per_window: window_stripes,
        })
    }

    fn affected_stripe_count(
        &self,
        start_body_lba: u64,
        end_body_lba: u64,
    ) -> Result<u64, ParityError> {
        let start_ordinal = self
            .object_first_ordinal
            .checked_add(start_body_lba)
            .ok_or(ParityError::Invariant(
                "bulk recovery start ordinal overflows",
            ))?;
        let end_ordinal =
            self.object_first_ordinal
                .checked_add(end_body_lba)
                .ok_or(ParityError::Invariant(
                    "bulk recovery end ordinal overflows",
                ))?;
        self.affected_stripe_count_for_ordinal_range(start_ordinal, end_ordinal)
    }

    fn affected_stripe_count_for_ordinal_range(
        &self,
        start_ordinal: u64,
        end_ordinal: u64,
    ) -> Result<u64, ParityError> {
        if start_ordinal >= end_ordinal {
            return Ok(0);
        }

        let stripes_per_epoch = u64::from(self.scheme.stripes_per_neighborhood);
        let data_shards_per_epoch = data_shards_per_epoch(&self.scheme)?;
        let first_epoch = start_ordinal / data_shards_per_epoch;
        let last_epoch = (end_ordinal - 1) / data_shards_per_epoch;

        if first_epoch == last_epoch {
            let length = end_ordinal
                .checked_sub(start_ordinal)
                .ok_or(ParityError::Invariant(
                    "bulk recovery ordinal range underflows",
                ))?;
            return Ok(length.min(stripes_per_epoch));
        }

        let first_epoch_len = data_shards_per_epoch - (start_ordinal % data_shards_per_epoch);
        let last_epoch_len = ((end_ordinal - 1) % data_shards_per_epoch) + 1;
        let middle_epochs = last_epoch
            .checked_sub(first_epoch)
            .and_then(|epoch_span| epoch_span.checked_sub(1))
            .ok_or(ParityError::Invariant(
                "bulk recovery epoch span underflows",
            ))?;

        let first_epoch_stripes = first_epoch_len.min(stripes_per_epoch);
        let middle_epoch_stripes = checked_mul(
            middle_epochs,
            stripes_per_epoch,
            "bulk recovery middle stripe count",
        )?;
        let last_epoch_stripes = last_epoch_len.min(stripes_per_epoch);

        checked_add(
            checked_add(
                first_epoch_stripes,
                middle_epoch_stripes,
                "bulk recovery leading stripe count",
            )?,
            last_epoch_stripes,
            "bulk recovery trailing stripe count",
        )
    }

    fn stripe_recovery_cache_bytes(&self) -> Result<u64, ParityError> {
        let shard_count = u64::from(self.scheme.data_blocks_per_stripe)
            .checked_add(u64::from(self.scheme.parity_blocks_per_stripe))
            .ok_or(ParityError::Invariant(
                "bulk recovery stripe shard count overflows",
            ))?;
        checked_mul(
            shard_count,
            u64::from(self.block_size),
            "bulk recovery stripe cache bytes",
        )
    }

    fn ensure_body_lba_in_range(&self, body_lba: u64) -> Result<(), ParityError> {
        if body_lba >= self.object_block_count {
            return Err(ParityError::FilemarkMapReconstruct(format!(
                "body LBA {body_lba} is outside tape file {} with block_count {}",
                self.tape_file_number, self.object_block_count
            )));
        }
        Ok(())
    }

    fn ordinal_range_to_body_region(
        &self,
        ordinals: Range<u64>,
    ) -> Result<(u64, u64), ParityError> {
        if ordinals.start > ordinals.end {
            return Err(ParityError::FilemarkMapReconstruct(format!(
                "ordinal recovery range start {} exceeds end {}",
                ordinals.start, ordinals.end
            )));
        }
        let object_end = self
            .object_first_ordinal
            .checked_add(self.object_block_count)
            .ok_or(ParityError::Invariant("object ordinal range overflows"))?;
        if ordinals.start == ordinals.end {
            if ordinals.start < self.object_first_ordinal || ordinals.start > object_end {
                return Err(ParityError::FilemarkMapReconstruct(format!(
                    "empty ordinal recovery range {}..{} is outside object tape file {} ordinal range {}..{}",
                    ordinals.start,
                    ordinals.end,
                    self.tape_file_number,
                    self.object_first_ordinal,
                    object_end
                )));
            }
            return Ok((ordinals.start - self.object_first_ordinal, 0));
        }

        let last_ordinal = ordinals.end.checked_sub(1).ok_or(ParityError::Invariant(
            "non-empty ordinal range has no tail",
        ))?;
        let start_position = self.scoped_map.map.position_for_ordinal(ordinals.start)?;
        let last_position = self.scoped_map.map.position_for_ordinal(last_ordinal)?;
        if start_position.tape_file_number != self.tape_file_number
            || last_position.tape_file_number != self.tape_file_number
        {
            return Err(ParityError::FilemarkMapReconstruct(format!(
                "ordinal recovery range {}..{} maps outside object tape file {}",
                ordinals.start, ordinals.end, self.tape_file_number
            )));
        }
        let block_count = ordinals
            .end
            .checked_sub(ordinals.start)
            .ok_or(ParityError::Invariant("ordinal recovery range underflows"))?;
        let end_body_lba = start_position
            .block_within_file
            .checked_add(block_count)
            .ok_or(ParityError::Invariant(
                "ordinal recovery body range overflows",
            ))?;
        if end_body_lba > self.object_block_count
            || last_position.block_within_file
                != end_body_lba.checked_sub(1).ok_or(ParityError::Invariant(
                    "ordinal recovery body range is unexpectedly empty",
                ))?
        {
            return Err(ParityError::FilemarkMapReconstruct(format!(
                "ordinal recovery range {}..{} is not contiguous within object tape file {}",
                ordinals.start, ordinals.end, self.tape_file_number
            )));
        }
        Ok((start_position.block_within_file, block_count))
    }

    fn locate_body_lba(&mut self, body_lba: u64) -> Result<(), ParityError> {
        if body_lba == self.object_block_count {
            let position = self.one_past_object_position()?;
            self.inner.locate_physical(position)?;
            self.cursor_body_lba = body_lba;
            return Ok(());
        }

        self.ensure_body_lba_in_range(body_lba)?;
        let physical = self.scoped_map.map.physical_position(TapeFilePosition {
            tape_file_number: self.tape_file_number,
            block_within_file: body_lba,
        })?;
        self.inner.locate_physical(physical)?;
        self.cursor_body_lba = body_lba;
        Ok(())
    }

    fn one_past_object_position(&self) -> Result<PhysicalPositionHint, ParityError> {
        let last = self
            .object_block_count
            .checked_sub(1)
            .ok_or(ParityError::Invariant("object block count is zero"))?;
        let last_physical = self.scoped_map.map.physical_position(TapeFilePosition {
            tape_file_number: self.tape_file_number,
            block_within_file: last,
        })?;
        Ok(PhysicalPositionHint {
            lba: last_physical.lba.saturating_add(1),
            partition: last_physical.partition,
        })
    }

    fn recovery_fallback_event_parts(
        &self,
        body_lba: u64,
    ) -> Option<(StripeAddress, Vec<StripePosition>)> {
        let ordinal = self
            .scoped_map
            .map
            .ordinal_at(TapeFilePosition {
                tape_file_number: self.tape_file_number,
                block_within_file: body_lba,
            })
            .ok()
            .flatten()?;
        let stripe = crate::mapping::ordinal_to_stripe(ordinal, &self.scheme).ok()?;
        let lost = vec![stripe.position];
        Some((stripe, lost))
    }

    fn emit_sidecar_recovery_event(
        &self,
        result: &SidecarRecoveryResult,
        outcome: RecoveryOutcome,
        body_lba: u64,
    ) {
        if let Some(hook) = self.audit_hook.as_ref() {
            hook.on_recovery(&RecoveryEvent {
                stripe: result.stripe,
                lost_blocks: result.lost_shards.clone(),
                outcome,
                at_lba_requested: body_lba,
                at_requested: (self.tape_file_number, body_lba),
            });
        }
    }

    fn emit_sidecar_metadata_health_event(&mut self, result: &SidecarRecoveryResult) {
        if !result.sidecar_metadata_health.is_degraded() {
            return;
        }
        let Some(hook) = self.audit_hook.as_ref() else {
            return;
        };
        let key = (
            result.sidecar_tape_file_number,
            result.stripe.neighborhood,
            result.sidecar_metadata_health,
        );
        if !self.reported_sidecar_metadata_health.insert(key) {
            return;
        }
        hook.on_sidecar_metadata_health(&SidecarMetadataHealthEvent {
            sidecar_tape_file_number: result.sidecar_tape_file_number,
            epoch_id: result.stripe.neighborhood,
            health: result.sidecar_metadata_health,
        });
    }

    fn emit_failed_sidecar_recovery_event(
        &self,
        fallback: Option<(StripeAddress, Vec<StripePosition>)>,
        err: &ParityError,
        body_lba: u64,
    ) {
        let Some(hook) = self.audit_hook.as_ref() else {
            return;
        };
        let Some((stripe, lost_blocks)) = fallback else {
            return;
        };
        let lost_count = match err {
            ParityError::Unrecoverable { lost_count, .. } => *lost_count,
            _ => lost_blocks.len() as u16,
        };
        hook.on_recovery(&RecoveryEvent {
            stripe,
            lost_blocks,
            outcome: RecoveryOutcome::Unrecoverable { lost_count },
            at_lba_requested: body_lba,
            at_requested: (self.tape_file_number, body_lba),
        });
    }

    fn read_record_with_transport_retry(
        &mut self,
        buf: &mut [u8],
        body_lba: u64,
    ) -> Result<RawReadOutcome, TapeIoError> {
        match self.inner.read_record(buf) {
            Err(ParityError::TapeIo(TapeIoError::Transport(_))) => {
                self.locate_body_lba(body_lba)
                    .map_err(parity_error_to_tape_io_error)?;
                self.inner
                    .read_record(buf)
                    .map_err(parity_error_to_tape_io_error)
            }
            other => other.map_err(parity_error_to_tape_io_error),
        }
    }

    fn adjacent_erasure_triggers_bulk_recovery(&self, body_lba: u64) -> bool {
        self.last_read_erasure_body_lba
            .and_then(|last| last.checked_add(1))
            == Some(body_lba)
    }

    fn probe_body_lba(&mut self, body_lba: u64) -> Result<NextBodyLbaProbe, ParityError> {
        if body_lba >= self.object_block_count {
            return Ok(NextBodyLbaProbe::NoErasure);
        }

        self.locate_body_lba(body_lba)?;
        let mut scratch = vec![0u8; self.block_size as usize];
        match self.read_record_with_transport_retry(&mut scratch, body_lba) {
            Ok(RawReadOutcome::Block { bytes, .. }) if bytes == self.block_size as usize => {
                Ok(NextBodyLbaProbe::CleanBlock {
                    body_lba,
                    data: scratch,
                })
            }
            Ok(RawReadOutcome::Block { .. }) => Ok(NextBodyLbaProbe::Erasure),
            Ok(RawReadOutcome::Filemark { .. }) | Ok(RawReadOutcome::EndOfData { .. }) => {
                Ok(NextBodyLbaProbe::NoErasure)
            }
            Err(err) if is_erasure(&err) => Ok(NextBodyLbaProbe::Erasure),
            Err(_) => Ok(NextBodyLbaProbe::NoErasure),
        }
    }

    fn auto_bulk_probe_block_limit(&self, policy: BulkRecoveryPolicy) -> Result<u64, ParityError> {
        let by_output_budget = policy.max_recovery_cache_bytes / u64::from(self.block_size);
        let parity_blocks = u64::from(self.scheme.parity_blocks_per_stripe);
        let by_policy_window = checked_mul(
            u64::from(policy.max_stripes_per_window),
            parity_blocks,
            "auto bulk policy block limit overflows",
        )?;
        let by_contiguous_tolerance = checked_mul(
            u64::from(self.scheme.stripes_per_neighborhood),
            parity_blocks,
            "auto bulk contiguous tolerance overflows",
        )?;
        Ok(by_output_budget
            .min(by_policy_window)
            .min(by_contiguous_tolerance)
            .max(1))
    }

    fn probe_auto_bulk_run(
        &mut self,
        body_lba: u64,
        policy: BulkRecoveryPolicy,
    ) -> Result<AutoBulkProbeRun, ParityError> {
        let remaining_blocks = self.object_block_count.saturating_sub(body_lba);
        let max_block_count = self
            .auto_bulk_probe_block_limit(policy)?
            .min(remaining_blocks)
            .max(1);
        let mut block_count = 1u64;
        let mut probe_body_lba = body_lba
            .checked_add(1)
            .ok_or(ParityError::Invariant("auto bulk probe body LBA overflows"))?;

        while block_count < max_block_count && probe_body_lba < self.object_block_count {
            match self.probe_body_lba(probe_body_lba)? {
                NextBodyLbaProbe::Erasure => {
                    block_count = block_count
                        .checked_add(1)
                        .ok_or(ParityError::Invariant("auto bulk block count overflows"))?;
                    probe_body_lba = probe_body_lba
                        .checked_add(1)
                        .ok_or(ParityError::Invariant("auto bulk probe body LBA overflows"))?;
                }
                NextBodyLbaProbe::CleanBlock { body_lba, data } => {
                    return Ok(AutoBulkProbeRun {
                        block_count,
                        clean_tail: Some((body_lba, data)),
                    });
                }
                NextBodyLbaProbe::NoErasure => break,
            }
        }

        Ok(AutoBulkProbeRun {
            block_count,
            clean_tail: None,
        })
    }

    fn recover_read_erasure(&mut self, body_lba: u64) -> Result<Vec<u8>, ParityError> {
        let recovered = if self.adjacent_erasure_triggers_bulk_recovery(body_lba) {
            self.recover_adjacent_erasure_region(body_lba)?
        } else {
            self.recover_block_at(body_lba)?
        };
        self.last_read_erasure_body_lba = Some(body_lba);
        Ok(recovered)
    }

    fn recover_adjacent_erasure_region(&mut self, body_lba: u64) -> Result<Vec<u8>, ParityError> {
        let policy = BulkRecoveryPolicy::default();
        let probe_run = self.probe_auto_bulk_run(body_lba, policy)?;
        let region = match self.recover_region_with_failure_audit(
            body_lba,
            probe_run.block_count,
            policy,
            false,
        ) {
            Ok(region) => region,
            Err(err @ ParityError::RecoveryPlanExceedsMemoryBudget { .. }) => return Err(err),
            Err(_) => {
                // Auto-escalation is an optimization; it must not make the
                // current block less recoverable than the isolated path.
                return self.recover_block_at(body_lba);
            }
        };
        let mut blocks = region.blocks.into_iter();
        let current = blocks.next().ok_or(ParityError::Invariant(
            "auto bulk recovery returned no current block",
        ))?;
        let mut next_body_lba = body_lba.checked_add(1).ok_or(ParityError::Invariant(
            "auto bulk recovery next body LBA overflows",
        ))?;
        for next in blocks {
            self.auto_read_blocks.insert(
                next_body_lba,
                BufferedAutoReadBlock {
                    data: next,
                    was_recovered: true,
                },
            );
            next_body_lba = next_body_lba.checked_add(1).ok_or(ParityError::Invariant(
                "auto bulk recovery next body LBA overflows",
            ))?;
        }
        if let Some((clean_body_lba, data)) = probe_run.clean_tail {
            if clean_body_lba != next_body_lba {
                return Err(ParityError::Invariant(
                    "auto bulk clean probe is not adjacent to recovered run",
                ));
            }
            let after_next = clean_body_lba.checked_add(1).ok_or(ParityError::Invariant(
                "auto clean probe next body LBA overflows",
            ))?;
            self.locate_body_lba(after_next)?;
            self.auto_read_blocks.insert(
                clean_body_lba,
                BufferedAutoReadBlock {
                    data,
                    was_recovered: false,
                },
            );
        }
        self.cursor_body_lba = body_lba
            .checked_add(1)
            .ok_or(ParityError::Invariant("body LBA cursor overflows"))?;
        Ok(current)
    }

    fn copy_recovered_block_to_read_buffer(
        &mut self,
        buf: &mut [u8],
        body_lba: u64,
        recovered: &[u8],
    ) -> Result<usize, TapeIoError> {
        if buf.len() < recovered.len() {
            return Err(TapeIoError::ReadBufferTooSmall {
                actual: recovered.len() as u32,
                provided: buf.len() as u32,
            });
        }
        buf[..recovered.len()].copy_from_slice(recovered);
        self.cursor_body_lba = body_lba.checked_add(1).ok_or_else(|| {
            TapeIoError::OperationFailed("object source cursor overflow".to_string())
        })?;
        Ok(recovered.len())
    }
}

impl<'a> BlockSource for ObjectParitySource<'a> {
    fn read_block(&mut self, buf: &mut [u8]) -> Result<usize, TapeIoError> {
        let body_lba = self.cursor_body_lba;
        self.ensure_body_lba_in_range(body_lba)
            .map_err(parity_error_to_tape_io_error)?;

        if let Some(buffered) = self.auto_read_blocks.get(&body_lba) {
            if buf.len() < buffered.data.len() {
                return Err(TapeIoError::ReadBufferTooSmall {
                    actual: buffered.data.len() as u32,
                    provided: buf.len() as u32,
                });
            }
        }
        if let Some(buffered) = self.auto_read_blocks.remove(&body_lba) {
            self.last_read_erasure_body_lba = buffered.was_recovered.then_some(body_lba);
            return self.copy_recovered_block_to_read_buffer(buf, body_lba, &buffered.data);
        }

        match self.read_record_with_transport_retry(buf, body_lba) {
            Ok(RawReadOutcome::Block { bytes, .. }) => {
                if bytes != self.block_size as usize {
                    return Err(TapeIoError::OperationFailed(format!(
                        "short fixed-block object read at body LBA {body_lba}: got {bytes}, expected {}",
                        self.block_size
                    )));
                }
                self.cursor_body_lba = self.cursor_body_lba.checked_add(1).ok_or_else(|| {
                    TapeIoError::OperationFailed("object source cursor overflow".to_string())
                })?;
                self.last_read_erasure_body_lba = None;
                Ok(bytes)
            }
            Ok(RawReadOutcome::Filemark { .. }) => Err(TapeIoError::OperationFailed(format!(
                "unexpected filemark inside object tape file {} at body LBA {body_lba}",
                self.tape_file_number
            ))),
            Ok(RawReadOutcome::EndOfData { .. }) => Err(TapeIoError::OperationFailed(format!(
                "unexpected EOD inside object tape file {} at body LBA {body_lba}",
                self.tape_file_number
            ))),
            Err(err) if !self.tar_only_unverified && is_erasure(&err) => {
                let recovered = self
                    .recover_read_erasure(body_lba)
                    .map_err(parity_error_to_tape_io_error)?;
                self.copy_recovered_block_to_read_buffer(buf, body_lba, &recovered)
            }
            Err(err) => Err(err),
        }
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        self.auto_read_blocks.clear();
        self.last_read_erasure_body_lba = None;
        self.locate_body_lba(lba)
            .map_err(parity_error_to_tape_io_error)?;
        Ok(body_position(self.cursor_body_lba, self.object_block_count))
    }

    fn space(&mut self, count: i64, kind: SpaceKind) -> Result<SpaceResult, TapeIoError> {
        if count == 0 {
            return Ok(SpaceResult {
                units_traversed: 0,
                stopped_at_boundary: self.cursor_body_lba == 0
                    || self.cursor_body_lba == self.object_block_count,
                position_after: body_position(self.cursor_body_lba, self.object_block_count),
            });
        }
        if kind != SpaceKind::Blocks {
            return Err(TapeIoError::OperationFailed(
                "ObjectParitySource supports only object-local block spacing".to_string(),
            ));
        }
        let target = if count >= 0 {
            self.cursor_body_lba
                .checked_add(count as u64)
                .ok_or_else(|| TapeIoError::OperationFailed("object space overflow".to_string()))?
        } else {
            self.cursor_body_lba
                .checked_sub(count.unsigned_abs())
                .ok_or_else(|| {
                    TapeIoError::OperationFailed("object space before BOF".to_string())
                })?
        };
        if target > self.object_block_count {
            return Err(TapeIoError::OperationFailed(format!(
                "object space target {target} exceeds block_count {}",
                self.object_block_count
            )));
        }
        self.auto_read_blocks.clear();
        self.last_read_erasure_body_lba = None;
        self.locate_body_lba(target)
            .map_err(parity_error_to_tape_io_error)?;
        Ok(SpaceResult {
            units_traversed: count,
            stopped_at_boundary: target == 0 || target == self.object_block_count,
            position_after: body_position(self.cursor_body_lba, self.object_block_count),
        })
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        Ok(body_position(self.cursor_body_lba, self.object_block_count))
    }
}

fn checked_mul(left: u64, right: u64, context: &'static str) -> Result<u64, ParityError> {
    left.checked_mul(right)
        .ok_or(ParityError::Invariant(context))
}

fn checked_add(left: u64, right: u64, context: &'static str) -> Result<u64, ParityError> {
    left.checked_add(right)
        .ok_or(ParityError::Invariant(context))
}

fn object_entry(
    scoped_map: &ScopedFilemarkMap,
    tape_file_number: u32,
) -> Result<&crate::filemark_map::TapeFileMapEntry, ParityError> {
    let entry = scoped_map
        .map
        .entries()
        .iter()
        .find(|entry| entry.tape_file_number == tape_file_number)
        .ok_or_else(|| {
            ParityError::FilemarkMapReconstruct(format!(
                "tape file {tape_file_number} is not described by the filemark map"
            ))
        })?;
    if entry.kind != TapeFileKind::Object {
        return Err(ParityError::FilemarkMapReconstruct(format!(
            "tape file {tape_file_number} is not an object tape file"
        )));
    }
    if entry.first_parity_data_ordinal.is_none() {
        return Err(ParityError::FilemarkMapReconstruct(format!(
            "object tape file {tape_file_number} is missing first ordinal"
        )));
    }
    Ok(entry)
}

fn body_position(body_lba: u64, object_block_count: u64) -> TapePosition {
    TapePosition {
        lba: body_lba,
        partition: 0,
        beginning_of_partition: body_lba == 0,
        end_of_partition: body_lba >= object_block_count,
        block_position_end_of_warning: false,
    }
}

fn validated_prefix_ordinals(scoped_map: &ScopedFilemarkMap) -> u64 {
    match &scoped_map.scope {
        crate::filemark_map::MapScope::Prefix {
            map_total_data_ordinals,
            ..
        } => *map_total_data_ordinals,
        crate::filemark_map::MapScope::Complete { .. } => scoped_map.map.total_data_ordinals(),
    }
}

fn parity_error_to_tape_io_error(err: ParityError) -> TapeIoError {
    match err {
        ParityError::TapeIo(err) => err,
        other => TapeIoError::OperationFailed(other.to_string()),
    }
}

/// Heuristic: is this error one the parity layer should attempt
/// to recover from? Per `docs/layer3c-design.md` §8.3.
///
/// MEDIUM_ERROR (sense key 0x03) always routes to recovery — LTO
/// ECC gave up on a sector.
///
/// Transport faults (timeouts, etc.) route to recovery only after the
/// object-source read path retries the same object LBA once per §9.1
/// ("Maybe — try once more; if it persists, treat as erasure").
///
/// Every other sense key — NOT_READY (0x02), HARDWARE_ERROR
/// (0x04), ILLEGAL_REQUEST (0x05), DATA_PROTECT (0x07), and the
/// rest — propagates as a drive error. Per codex idref=d4e7492e
/// (Medium): the prior HARDWARE_ERROR "positioning ASC"
/// allowlist (0x09/0x15/0x3B/0x52) is not grounded in the IBM
/// LTO SCSI Reference GA32-0928-08 — Annex B Table B.5 lists
/// 03/02, 04/03, 10/01, 40/XX, 41/00, 44/00, 51/00, 52/00, 53/00,
/// 53/04, EE/0E, EE/0F as the real Hardware Error tuples
/// (52/00 being CARTRIDGE FAULT, not target-LBA servo damage).
/// 0x09, 0x15, and 0x3B appear under Medium Error (Table B.4).
/// Until a Hardware Error tuple is demonstrably equivalent to a
/// per-block erasure on real hardware, HARDWARE_ERROR
/// propagates rather than silently triggering reconstruction.
/// Same reasoning applies to NOT_READY: Annex B Table B.3
/// entries are drive-state codes, not per-block faults.
fn is_erasure(err: &TapeIoError) -> bool {
    match err {
        TapeIoError::CheckCondition(ScsiError::CheckCondition { sense, .. }) => {
            decode_sense(sense).is_some_and(|decoded| decoded.key == 0x03)
        }
        TapeIoError::Transport(_) => true,
        _ => false,
    }
}

#[cfg(test)]
mod erasure_tests {
    use super::*;

    fn cc(key: u8, asc: u8) -> TapeIoError {
        let mut sense = vec![0u8; 32];
        sense[0] = 0x70;
        sense[2] = key;
        sense[7] = 24;
        sense[12] = asc;
        TapeIoError::CheckCondition(ScsiError::CheckCondition {
            sense,
            bytes_transferred: 0,
        })
    }

    #[test]
    fn is_erasure_rejects_not_ready_entirely() {
        for asc in [0x00u8, 0x04, 0x3A, 0x3E, 0x53, 0x3B, 0x44, 0x52] {
            assert!(
                !is_erasure(&cc(0x02, asc)),
                "NOT_READY + ASC {asc:#x} must propagate, not route to recovery"
            );
        }
    }

    #[test]
    fn is_erasure_rejects_all_hardware_error_tuples() {
        for asc in [
            0x00u8, 0x03, 0x04, 0x09, 0x10, 0x15, 0x3B, 0x40, 0x41, 0x44, 0x51, 0x52, 0x53, 0xEE,
        ] {
            assert!(
                !is_erasure(&cc(0x04, asc)),
                "HARDWARE_ERROR + ASC {asc:#x} must propagate"
            );
        }
    }

    #[test]
    fn is_erasure_accepts_medium_error_and_transport() {
        for asc in [0x00u8, 0x11, 0x14] {
            assert!(is_erasure(&cc(0x03, asc)));
        }
        assert!(is_erasure(&TapeIoError::CheckCondition(
            ScsiError::CheckCondition {
                sense: vec![0x72, 0x03, 0x11, 0x00],
                bytes_transferred: 0,
            },
        )));
        assert!(is_erasure(&TapeIoError::Transport(
            ScsiError::TransportError {
                status: 0,
                host_status: 0,
                driver_status: 0,
                info: 0,
                sense: Vec::new(),
            },
        )));
    }
}

#[cfg(test)]
mod object_source_tests {
    use super::*;
    use crate::codec::ReedSolomonCodec;
    use crate::filemark_map::{FilemarkMap, MapScope, TapeFileMapEntry, TapeFilePosition};
    use crate::mapping::ordinal_to_stripe;
    use crate::model::{SchemeId, SidecarMetadataHealth, SidecarMetadataHealthEvent};
    use crate::sidecar::{data_shard_crc64, encode_sidecar_tape_file, SidecarDescriptor};
    use std::collections::{BTreeMap, BTreeSet};
    use std::ops::Range;
    use std::sync::Mutex;

    const TAPE_UUID: [u8; 16] = [0x44; 16];
    const BLOCK_SIZE: u32 = 256;
    const MIB: u64 = 1024 * 1024;

    #[derive(Clone, Debug)]
    enum Record {
        Block(Vec<u8>),
        Filemark,
    }

    #[derive(Debug)]
    struct RawVec {
        records: Vec<Record>,
        cursor: usize,
        configured_block_size: Option<u32>,
        medium_error_lbas: Vec<usize>,
        transient_transport_lbas: Vec<usize>,
        check_condition_lbas: Vec<(usize, u8, u8, u8)>,
        read_lbas: Vec<usize>,
    }

    impl RawVec {
        fn new(records: Vec<Record>) -> Self {
            Self {
                records,
                cursor: 0,
                configured_block_size: None,
                medium_error_lbas: Vec::new(),
                transient_transport_lbas: Vec::new(),
                check_condition_lbas: Vec::new(),
                read_lbas: Vec::new(),
            }
        }
    }

    impl RawTapeSource for RawVec {
        fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), ParityError> {
            if block_size == 0 {
                return Err(ParityError::Invariant("test block size is zero"));
            }
            self.configured_block_size = Some(block_size);
            Ok(())
        }

        fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
            self.cursor = usize::try_from(hint.lba)
                .map_err(|_| ParityError::Invariant("test LBA overflows usize"))?;
            Ok(())
        }

        fn space_filemarks(
            &mut self,
            _count: i64,
        ) -> Result<crate::raw::SpaceFilemarksOutcome, ParityError> {
            Err(ParityError::Invariant("object source test does not space"))
        }

        fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
            self.read_lbas.push(self.cursor);
            if let Some(index) = self
                .transient_transport_lbas
                .iter()
                .position(|lba| *lba == self.cursor)
            {
                self.transient_transport_lbas.remove(index);
                return Err(ParityError::TapeIo(transport_error()));
            }
            if let Some((_, key, asc, ascq)) = self
                .check_condition_lbas
                .iter()
                .find(|(lba, _, _, _)| *lba == self.cursor)
            {
                return Err(ParityError::TapeIo(check_condition(*key, *asc, *ascq)));
            }
            if self.medium_error_lbas.contains(&self.cursor) {
                return Err(ParityError::TapeIo(medium_error()));
            }
            let Some(record) = self.records.get(self.cursor) else {
                return Ok(RawReadOutcome::EndOfData {
                    position_after: PhysicalPositionHint::new(self.cursor as u64),
                });
            };
            match record {
                Record::Block(block) => {
                    let bytes = block.len();
                    buf[..bytes].copy_from_slice(block);
                    self.cursor += 1;
                    Ok(RawReadOutcome::Block {
                        bytes,
                        position_after: PhysicalPositionHint::new(self.cursor as u64),
                    })
                }
                Record::Filemark => {
                    self.cursor += 1;
                    Ok(RawReadOutcome::Filemark {
                        position_after: PhysicalPositionHint::new(self.cursor as u64),
                    })
                }
            }
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            Ok(PhysicalPositionHint::new(self.cursor as u64))
        }
    }

    #[derive(Debug)]
    struct RawBorrowedTape<'a> {
        object_blocks: &'a [Vec<u8>],
        sidecar_blocks: &'a [Vec<u8>],
        cursor: usize,
        configured_block_size: Option<u32>,
        read_lbas: Vec<usize>,
    }

    impl<'a> RawBorrowedTape<'a> {
        fn new(object_blocks: &'a [Vec<u8>], sidecar_blocks: &'a [Vec<u8>]) -> Self {
            Self {
                object_blocks,
                sidecar_blocks,
                cursor: 0,
                configured_block_size: None,
                read_lbas: Vec::new(),
            }
        }

        fn object_start(&self) -> usize {
            2
        }

        fn object_end(&self) -> usize {
            self.object_start() + self.object_blocks.len()
        }

        fn sidecar_start(&self) -> usize {
            self.object_end() + 1
        }

        fn sidecar_end(&self) -> usize {
            self.sidecar_start() + self.sidecar_blocks.len()
        }

        fn copy_block(buf: &mut [u8], block: &[u8]) -> Result<usize, ParityError> {
            if buf.len() < block.len() {
                return Err(ParityError::Invariant("test read buffer is too small"));
            }
            buf[..block.len()].copy_from_slice(block);
            Ok(block.len())
        }
    }

    impl RawTapeSource for RawBorrowedTape<'_> {
        fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), ParityError> {
            if block_size == 0 {
                return Err(ParityError::Invariant("test block size is zero"));
            }
            self.configured_block_size = Some(block_size);
            Ok(())
        }

        fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
            self.cursor = usize::try_from(hint.lba)
                .map_err(|_| ParityError::Invariant("test LBA overflows usize"))?;
            Ok(())
        }

        fn space_filemarks(
            &mut self,
            _count: i64,
        ) -> Result<crate::raw::SpaceFilemarksOutcome, ParityError> {
            Err(ParityError::Invariant("object source test does not space"))
        }

        fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
            self.read_lbas.push(self.cursor);
            let bytes = if self.cursor == 0 {
                let block_size = self.configured_block_size.unwrap_or(BLOCK_SIZE);
                let bootstrap = vec![0xB0; block_size as usize];
                Self::copy_block(buf, &bootstrap)?
            } else if self.cursor == 1 || self.cursor == self.object_end() {
                self.cursor += 1;
                return Ok(RawReadOutcome::Filemark {
                    position_after: PhysicalPositionHint::new(self.cursor as u64),
                });
            } else if (self.object_start()..self.object_end()).contains(&self.cursor) {
                let index = self.cursor - self.object_start();
                Self::copy_block(buf, &self.object_blocks[index])?
            } else if (self.sidecar_start()..self.sidecar_end()).contains(&self.cursor) {
                let index = self.cursor - self.sidecar_start();
                Self::copy_block(buf, &self.sidecar_blocks[index])?
            } else if self.cursor == self.sidecar_end() {
                self.cursor += 1;
                return Ok(RawReadOutcome::Filemark {
                    position_after: PhysicalPositionHint::new(self.cursor as u64),
                });
            } else {
                return Ok(RawReadOutcome::EndOfData {
                    position_after: PhysicalPositionHint::new(self.cursor as u64),
                });
            };

            self.cursor += 1;
            Ok(RawReadOutcome::Block {
                bytes,
                position_after: PhysicalPositionHint::new(self.cursor as u64),
            })
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            Ok(PhysicalPositionHint::new(self.cursor as u64))
        }
    }

    fn medium_error() -> TapeIoError {
        check_condition(0x03, 0x11, 0x00)
    }

    fn transport_error() -> TapeIoError {
        TapeIoError::Transport(ScsiError::TransportError {
            status: 0,
            host_status: 0,
            driver_status: 0,
            info: 0,
            sense: Vec::new(),
        })
    }

    fn check_condition(key: u8, asc: u8, ascq: u8) -> TapeIoError {
        let mut sense = vec![0u8; 32];
        sense[0] = 0x70;
        sense[2] = key;
        sense[7] = 24;
        sense[12] = asc;
        sense[13] = ascq;
        TapeIoError::CheckCondition(ScsiError::CheckCondition {
            sense,
            bytes_transferred: 0,
        })
    }

    fn scheme() -> ParityScheme {
        ParityScheme {
            id: SchemeId::new_static("object-source-test"),
            data_blocks_per_stripe: 2,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 2,
        }
    }

    fn bulk_scheme() -> ParityScheme {
        ParityScheme {
            id: SchemeId::new_static("object-source-bulk-test"),
            data_blocks_per_stripe: 4,
            parity_blocks_per_stripe: 2,
            stripes_per_neighborhood: 2,
        }
    }

    fn wide_bulk_scheme() -> ParityScheme {
        ParityScheme {
            id: SchemeId::new_static("object-source-wide-bulk-test"),
            data_blocks_per_stripe: 3,
            parity_blocks_per_stripe: 2,
            stripes_per_neighborhood: 3,
        }
    }

    fn media_scale_scheme(stripes_per_neighborhood: u32) -> ParityScheme {
        ParityScheme {
            id: SchemeId::new_static("object-source-media-scale-test"),
            data_blocks_per_stripe: 4,
            parity_blocks_per_stripe: 2,
            stripes_per_neighborhood,
        }
    }

    fn block(seed: u8) -> Vec<u8> {
        let mut block = vec![seed; BLOCK_SIZE as usize];
        block[0] = seed.wrapping_mul(17);
        block[1] = seed.wrapping_mul(29);
        block
    }

    fn media_block(ordinal: u64, block_size: u32) -> Vec<u8> {
        let fill = ordinal.wrapping_mul(37).wrapping_add(0xA5) as u8;
        let mut block = vec![fill; block_size as usize];
        let ordinal_bytes = ordinal.to_le_bytes();
        let inverse_bytes = (!ordinal).to_le_bytes();
        let first_len = block.len().min(ordinal_bytes.len());
        block[..first_len].copy_from_slice(&ordinal_bytes[..first_len]);
        if block.len() > ordinal_bytes.len() {
            let end = (ordinal_bytes.len() + inverse_bytes.len()).min(block.len());
            block[ordinal_bytes.len()..end]
                .copy_from_slice(&inverse_bytes[..end - ordinal_bytes.len()]);
        }
        block
    }

    fn media_scale_object_blocks(scheme: &ParityScheme, block_size: u32) -> Vec<Vec<u8>> {
        let object_block_count =
            u64::from(scheme.data_blocks_per_stripe) * u64::from(scheme.stripes_per_neighborhood);
        (0..object_block_count)
            .map(|ordinal| media_block(ordinal, block_size))
            .collect()
    }

    fn sidecar_for_object(
        scheme: &ParityScheme,
        object_blocks: &[Vec<u8>],
    ) -> crate::sidecar::EncodedSidecarTapeFile {
        sidecar_for_object_with_block_size(scheme, object_blocks, BLOCK_SIZE)
    }

    fn sidecar_for_object_with_block_size(
        scheme: &ParityScheme,
        object_blocks: &[Vec<u8>],
        block_size: u32,
    ) -> crate::sidecar::EncodedSidecarTapeFile {
        let codec = ReedSolomonCodec::new(scheme).unwrap();
        let mut parity_shards = Vec::new();
        for stripe in 0..scheme.stripes_per_neighborhood as usize {
            let mut data = Vec::new();
            for row in 0..scheme.data_blocks_per_stripe as usize {
                let ordinal = row * scheme.stripes_per_neighborhood as usize + stripe;
                data.push(object_blocks[ordinal].clone());
            }
            parity_shards.extend(codec.encode(&data).unwrap());
        }
        let descriptor = SidecarDescriptor {
            tape_uuid: TAPE_UUID,
            epoch_id: 0,
            k: scheme.data_blocks_per_stripe,
            m: scheme.parity_blocks_per_stripe,
            stripes_per_epoch: scheme.stripes_per_neighborhood,
            block_size,
            protected_ordinal_start: 0,
            protected_ordinal_end_exclusive: object_blocks.len() as u64,
        };
        let data_crcs = object_blocks
            .iter()
            .map(|block| data_shard_crc64(block))
            .collect();
        encode_sidecar_tape_file(&descriptor, &parity_shards, data_crcs).unwrap()
    }

    fn scoped_map(sidecar_blocks: u64, object_blocks: u64) -> ScopedFilemarkMap {
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, object_blocks, 0),
            TapeFileMapEntry::parity_sidecar(2, sidecar_blocks, 0, 0, object_blocks),
        ])
        .unwrap();
        ScopedFilemarkMap::from_catalog(map, object_blocks)
    }

    #[test]
    fn affected_stripe_count_uses_epoch_arithmetic_for_offset_object_ranges() {
        let scheme = ParityScheme {
            id: SchemeId::new_static("object-source-stripe-count-test"),
            data_blocks_per_stripe: 3,
            parity_blocks_per_stripe: 2,
            stripes_per_neighborhood: 4,
        };
        let prefix_blocks = (1..=5).map(block).collect::<Vec<_>>();
        let object_blocks = (6..=25).map(block).collect::<Vec<_>>();
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, prefix_blocks.len() as u64, 0),
            TapeFileMapEntry::object(2, object_blocks.len() as u64, prefix_blocks.len() as u64),
        ])
        .expect("offset object map validates");
        let scoped = ScopedFilemarkMap::from_catalog(
            map,
            (prefix_blocks.len() + object_blocks.len()) as u64,
        );

        let mut records = Vec::new();
        records.push(Record::Block(vec![0xB0; BLOCK_SIZE as usize]));
        records.push(Record::Filemark);
        records.extend(prefix_blocks.iter().cloned().map(Record::Block));
        records.push(Record::Filemark);
        records.extend(object_blocks.iter().cloned().map(Record::Block));
        records.push(Record::Filemark);
        let mut raw = RawVec::new(records);

        let source = ObjectParitySource::open(
            &mut raw,
            scheme,
            TAPE_UUID,
            scoped,
            BLOCK_SIZE,
            2,
            OpenTrust::RequireValidated,
        )
        .expect("offset object source opens");

        assert_eq!(source.affected_stripe_count(0, 1).unwrap(), 1);
        assert_eq!(source.affected_stripe_count(0, 3).unwrap(), 3);
        assert_eq!(source.affected_stripe_count(0, 5).unwrap(), 4);
        assert_eq!(source.affected_stripe_count(5, 7).unwrap(), 2);
        assert_eq!(source.affected_stripe_count(5, 9).unwrap(), 4);
        assert_eq!(source.affected_stripe_count(7, 17).unwrap(), 4);
        assert_eq!(source.affected_stripe_count(0, 20).unwrap(), 9);
    }

    fn raw_tape(object_blocks: &[Vec<u8>], sidecar_blocks: &[Vec<u8>]) -> RawVec {
        let mut records = Vec::new();
        records.push(Record::Block(vec![0xB0; BLOCK_SIZE as usize]));
        records.push(Record::Filemark);
        for block in object_blocks {
            records.push(Record::Block(block.clone()));
        }
        records.push(Record::Filemark);
        for block in sidecar_blocks {
            records.push(Record::Block(block.clone()));
        }
        records.push(Record::Filemark);
        RawVec::new(records)
    }

    fn media_scale_policy(
        block_count: u64,
        block_size: u32,
        scheme: &ParityScheme,
        max_stripes_per_window: u32,
    ) -> BulkRecoveryPolicy {
        let output_bytes = block_count * u64::from(block_size);
        let stripe_cache_bytes = (u64::from(scheme.data_blocks_per_stripe)
            + u64::from(scheme.parity_blocks_per_stripe))
            * u64::from(block_size);
        BulkRecoveryPolicy {
            max_recovery_cache_bytes: output_bytes
                + stripe_cache_bytes * u64::from(max_stripes_per_window),
            allow_windowed_recovery: true,
            max_stripes_per_window,
        }
    }

    fn requested_indexes_by_stripe(
        ordinals: Range<u64>,
        scheme: &ParityScheme,
    ) -> BTreeMap<u32, BTreeSet<u16>> {
        let mut requested = BTreeMap::<u32, BTreeSet<u16>>::new();
        for ordinal in ordinals {
            let stripe = ordinal_to_stripe(ordinal, scheme).expect("ordinal maps to a stripe");
            let StripePosition::Data { index } = stripe.position else {
                panic!("test ordinal unexpectedly mapped to parity");
            };
            requested
                .entry(stripe.stripe_index)
                .or_default()
                .insert(index);
        }
        requested
    }

    fn assert_media_scale_peer_reads_once(
        raw: &RawBorrowedTape<'_>,
        scoped: &ScopedFilemarkMap,
        sidecar: &crate::sidecar::EncodedSidecarTapeFile,
        scheme: &ParityScheme,
        requested_ordinals: Range<u64>,
    ) {
        let requested = requested_indexes_by_stripe(requested_ordinals, scheme);
        let sidecar_header_blocks = sidecar.header.shard_index_block_count;
        for (stripe_index, requested_indexes) in requested {
            for data_index in 0..scheme.data_blocks_per_stripe {
                if requested_indexes.contains(&data_index) {
                    continue;
                }
                let ordinal = u64::from(data_index) * u64::from(scheme.stripes_per_neighborhood)
                    + u64::from(stripe_index);
                let position = scoped
                    .map
                    .position_for_ordinal(ordinal)
                    .expect("media-scale data peer ordinal is mapped");
                let lba = scoped
                    .map
                    .physical_position(position)
                    .expect("media-scale data peer has a physical position")
                    .lba as usize;
                assert_eq!(
                    raw.read_lbas.iter().filter(|read| **read == lba).count(),
                    1,
                    "data peer ordinal {ordinal} must be read exactly once"
                );
            }

            for parity_index in 0..scheme.parity_blocks_per_stripe {
                let parity_entry_index = usize::try_from(
                    u64::from(stripe_index) * u64::from(scheme.parity_blocks_per_stripe)
                        + u64::from(parity_index),
                )
                .expect("parity entry index fits usize");
                let lba = scoped
                    .map
                    .physical_position(TapeFilePosition {
                        tape_file_number: 2,
                        block_within_file: u64::from(sidecar_header_blocks)
                            + u64::try_from(parity_entry_index)
                                .expect("parity entry index fits u64"),
                    })
                    .expect("media-scale parity peer has a physical position")
                    .lba as usize;
                assert_eq!(
                    raw.read_lbas.iter().filter(|read| **read == lba).count(),
                    1,
                    "parity peer stripe {stripe_index} index {parity_index} must be read exactly once"
                );
            }
        }
    }

    fn run_media_scale_recover_region_success(
        region_mib: u64,
        stripes_per_neighborhood: u32,
        max_stripes_per_window: u32,
    ) {
        let block_size = MIB as u32;
        let block_count = region_mib;
        let scheme = media_scale_scheme(stripes_per_neighborhood);
        assert_eq!(
            u64::from(scheme.stripes_per_neighborhood) * u64::from(scheme.parity_blocks_per_stripe),
            block_count,
            "success fixture should request exactly the contiguous tolerance"
        );
        let object_blocks = media_scale_object_blocks(&scheme, block_size);
        let sidecar = sidecar_for_object_with_block_size(&scheme, &object_blocks, block_size);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let mut raw = RawBorrowedTape::new(&object_blocks, &sidecar.blocks);

        {
            let mut source = ObjectParitySource::open(
                &mut raw,
                scheme.clone(),
                TAPE_UUID,
                scoped.clone(),
                block_size,
                1,
                OpenTrust::RequireValidated,
            )
            .expect("media-scale object source opens");

            let recovered = source
                .recover_region(
                    0,
                    block_count,
                    media_scale_policy(block_count, block_size, &scheme, max_stripes_per_window),
                )
                .expect("media-scale bulk region recovers");
            assert_eq!(recovered.start_body_lba, 0);
            assert_eq!(recovered.blocks.len(), block_count as usize);
            for (offset, recovered_block) in recovered.blocks.iter().enumerate() {
                assert_eq!(
                    recovered_block, &object_blocks[offset],
                    "media-scale recovered block {offset} must match original"
                );
            }
        }

        assert_media_scale_peer_reads_once(&raw, &scoped, &sidecar, &scheme, 0..block_count);
    }

    #[test]
    fn object_source_clean_read_is_body_lba_passthrough() {
        let object_blocks = vec![block(1), block(2)];
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, 2, 0),
        ])
        .unwrap();
        let scoped = ScopedFilemarkMap::from_catalog(map, 0);
        let mut raw = raw_tape(&object_blocks, &[]);
        let mut source = ObjectParitySource::open(
            &mut raw,
            scheme(),
            TAPE_UUID,
            scoped,
            BLOCK_SIZE,
            1,
            OpenTrust::RequireValidated,
        )
        .expect("object source opens");

        source.locate(1).expect("locate body LBA");
        let mut buf = vec![0u8; BLOCK_SIZE as usize];
        let bytes = source.read_block(&mut buf).expect("clean object read");

        assert_eq!(bytes, BLOCK_SIZE as usize);
        assert_eq!(buf, object_blocks[1]);
        assert_eq!(source.position().unwrap().lba, 2);
        drop(source);
        assert_eq!(raw.configured_block_size, Some(BLOCK_SIZE));
        assert_eq!(raw.position().unwrap(), PhysicalPositionHint::new(4));
    }

    #[test]
    fn object_source_read_error_recovers_and_audits_object_address() {
        struct Collector(Mutex<Vec<RecoveryEvent>>);
        impl ParityAuditHook for Collector {
            fn on_recovery(&self, event: &RecoveryEvent) {
                self.0.lock().unwrap().push(event.clone());
            }
        }

        let clean_scheme = scheme();
        let object_blocks = vec![block(1), block(2), block(3), block(4)];
        let sidecar = sidecar_for_object(&clean_scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
        raw.medium_error_lbas.push(4); // tape_file 1, body_lba 2.
        let collector = Arc::new(Collector(Mutex::new(Vec::new())));

        let mut source = ObjectParitySource::open(
            &mut raw,
            clean_scheme,
            TAPE_UUID,
            scoped,
            BLOCK_SIZE,
            1,
            OpenTrust::RequireValidated,
        )
        .expect("object source opens");
        source.set_audit_hook(Some(collector.clone() as Arc<dyn ParityAuditHook>));

        source.locate(2).expect("locate failed body block");
        let mut buf = vec![0u8; BLOCK_SIZE as usize];
        let bytes = source
            .read_block(&mut buf)
            .expect("read error routes to sidecar recovery");

        assert_eq!(bytes, BLOCK_SIZE as usize);
        assert_eq!(buf, object_blocks[2]);
        assert_eq!(source.position().unwrap().lba, 3);
        drop(source);
        assert_eq!(raw.position().unwrap(), PhysicalPositionHint::new(5));

        let events = collector.0.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].outcome, RecoveryOutcome::Recovered));
        assert_eq!(events[0].at_requested, (1, 2));
        assert_eq!(events[0].at_lba_requested, 2);
        assert_eq!(
            events[0].lost_blocks,
            vec![StripePosition::Data { index: 1 }]
        );
    }

    #[test]
    fn object_source_transport_error_retries_once_before_recovery() {
        struct Collector(Mutex<Vec<RecoveryEvent>>);
        impl ParityAuditHook for Collector {
            fn on_recovery(&self, event: &RecoveryEvent) {
                self.0.lock().unwrap().push(event.clone());
            }
        }

        let clean_scheme = scheme();
        let object_blocks = vec![block(1), block(2), block(3), block(4)];
        let sidecar = sidecar_for_object(&clean_scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
        raw.transient_transport_lbas.push(4); // tape_file 1, body_lba 2.
        let collector = Arc::new(Collector(Mutex::new(Vec::new())));

        let mut source = ObjectParitySource::open(
            &mut raw,
            clean_scheme,
            TAPE_UUID,
            scoped,
            BLOCK_SIZE,
            1,
            OpenTrust::RequireValidated,
        )
        .expect("object source opens");
        source.set_audit_hook(Some(collector.clone() as Arc<dyn ParityAuditHook>));

        source.locate(2).expect("locate failed body block");
        let mut buf = vec![0u8; BLOCK_SIZE as usize];
        let bytes = source
            .read_block(&mut buf)
            .expect("transient transport error is retried as a clean read");

        assert_eq!(bytes, BLOCK_SIZE as usize);
        assert_eq!(buf, object_blocks[2]);
        assert_eq!(source.position().unwrap().lba, 3);
        drop(source);
        assert_eq!(
            &raw.read_lbas[..2],
            &[4, 4],
            "transport retry must re-read the same physical LBA once"
        );
        assert_eq!(raw.position().unwrap(), PhysicalPositionHint::new(5));
        assert!(
            collector.0.lock().unwrap().is_empty(),
            "a successful retry must not emit sidecar recovery audit events"
        );

        let retry_scheme = scheme();
        let sidecar = sidecar_for_object(&retry_scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
        raw.transient_transport_lbas.extend([4, 4]); // retry also fails.
        let collector = Arc::new(Collector(Mutex::new(Vec::new())));

        let mut source = ObjectParitySource::open(
            &mut raw,
            retry_scheme,
            TAPE_UUID,
            scoped,
            BLOCK_SIZE,
            1,
            OpenTrust::RequireValidated,
        )
        .expect("object source opens");
        source.set_audit_hook(Some(collector.clone() as Arc<dyn ParityAuditHook>));

        source.locate(2).expect("locate failed body block");
        let mut buf = vec![0u8; BLOCK_SIZE as usize];
        let bytes = source
            .read_block(&mut buf)
            .expect("persistent transport error falls back to sidecar recovery");

        assert_eq!(bytes, BLOCK_SIZE as usize);
        assert_eq!(buf, object_blocks[2]);
        assert_eq!(source.position().unwrap().lba, 3);
        drop(source);
        assert_eq!(
            &raw.read_lbas[..2],
            &[4, 4],
            "recovery must start only after the one transport retry also fails"
        );
        assert_eq!(raw.position().unwrap(), PhysicalPositionHint::new(5));
        let events = collector.0.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].outcome, RecoveryOutcome::Recovered));
        assert_eq!(events[0].at_requested, (1, 2));
    }

    #[test]
    fn adjacent_read_errors_auto_escalate_to_bulk_region_and_buffer_next_block() {
        struct Collector(Mutex<Vec<RecoveryEvent>>);
        impl ParityAuditHook for Collector {
            fn on_recovery(&self, event: &RecoveryEvent) {
                self.0.lock().unwrap().push(event.clone());
            }
        }

        let scheme = bulk_scheme();
        let object_blocks = (1..=8).map(block).collect::<Vec<_>>();
        let sidecar = sidecar_for_object(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
        raw.medium_error_lbas.extend([2, 3, 4]); // body_lba 0, 1, 2.
        let collector = Arc::new(Collector(Mutex::new(Vec::new())));

        let mut source = ObjectParitySource::open(
            &mut raw,
            scheme,
            TAPE_UUID,
            scoped,
            BLOCK_SIZE,
            1,
            OpenTrust::RequireValidated,
        )
        .expect("object source opens");
        source.set_audit_hook(Some(collector.clone() as Arc<dyn ParityAuditHook>));

        let mut buf = vec![0u8; BLOCK_SIZE as usize];
        let bytes = source
            .read_block(&mut buf)
            .expect("first erasure uses isolated recovery");
        assert_eq!(bytes, BLOCK_SIZE as usize);
        assert_eq!(buf, object_blocks[0]);
        assert_eq!(source.position().unwrap().lba, 1);
        assert_eq!(collector.0.lock().unwrap().len(), 1);

        let bytes = source
            .read_block(&mut buf)
            .expect("adjacent erasure escalates to bulk recovery");
        assert_eq!(bytes, BLOCK_SIZE as usize);
        assert_eq!(buf, object_blocks[1]);
        assert_eq!(source.position().unwrap().lba, 2);
        {
            let events = collector.0.lock().unwrap();
            assert_eq!(
                events.len(),
                3,
                "bulk escalation should recover and audit the probed adjacent block"
            );
            assert_eq!(events[1].at_requested, (1, 1));
            assert_eq!(events[2].at_requested, (1, 2));
        }

        let bytes = source
            .read_block(&mut buf)
            .expect("probed adjacent erasure is returned from the recovery buffer");
        assert_eq!(bytes, BLOCK_SIZE as usize);
        assert_eq!(buf, object_blocks[2]);
        assert_eq!(source.position().unwrap().lba, 3);
        assert_eq!(
            collector.0.lock().unwrap().len(),
            3,
            "serving the buffered block must not emit a second recovery event"
        );
        drop(source);

        assert_eq!(
            raw.read_lbas.iter().filter(|read| **read == 4).count(),
            2,
            "body_lba 2 should be read once as the first stripe's damaged peer, probed once, and then served from the auto bulk buffer"
        );
        assert_eq!(raw.position().unwrap(), PhysicalPositionHint::new(6));
    }

    #[test]
    fn sustained_adjacent_read_errors_expand_auto_bulk_window() {
        struct Collector(Mutex<Vec<RecoveryEvent>>);
        impl ParityAuditHook for Collector {
            fn on_recovery(&self, event: &RecoveryEvent) {
                self.0.lock().unwrap().push(event.clone());
            }
        }

        let scheme = bulk_scheme();
        let object_blocks = (1..=8).map(block).collect::<Vec<_>>();
        let sidecar = sidecar_for_object(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
        raw.medium_error_lbas.extend([2, 3, 4, 5]); // body_lba 0, 1, 2, 3.
        let collector = Arc::new(Collector(Mutex::new(Vec::new())));

        let mut source = ObjectParitySource::open(
            &mut raw,
            scheme,
            TAPE_UUID,
            scoped,
            BLOCK_SIZE,
            1,
            OpenTrust::RequireValidated,
        )
        .expect("object source opens");
        source.set_audit_hook(Some(collector.clone() as Arc<dyn ParityAuditHook>));

        let mut buf = vec![0u8; BLOCK_SIZE as usize];
        source
            .read_block(&mut buf)
            .expect("first erasure uses isolated recovery");
        assert_eq!(buf, object_blocks[0]);
        assert_eq!(collector.0.lock().unwrap().len(), 1);

        source
            .read_block(&mut buf)
            .expect("second adjacent erasure expands to a sustained bulk run");
        assert_eq!(buf, object_blocks[1]);
        assert_eq!(source.position().unwrap().lba, 2);
        {
            let events = collector.0.lock().unwrap();
            assert_eq!(
                events.len(),
                4,
                "expanded bulk recovery should recover body_lba 1, 2, and 3 together"
            );
            assert_eq!(events[1].at_requested, (1, 1));
            assert_eq!(events[2].at_requested, (1, 2));
            assert_eq!(events[3].at_requested, (1, 3));
        }

        source
            .read_block(&mut buf)
            .expect("first expanded-run block is buffered");
        assert_eq!(buf, object_blocks[2]);
        source
            .read_block(&mut buf)
            .expect("second expanded-run block is buffered");
        assert_eq!(buf, object_blocks[3]);
        source
            .read_block(&mut buf)
            .expect("clean tail probe is buffered without a recovery event");
        assert_eq!(buf, object_blocks[4]);
        assert_eq!(source.position().unwrap().lba, 5);
        assert_eq!(
            collector.0.lock().unwrap().len(),
            4,
            "serving expanded-run buffers must not emit duplicate recovery events"
        );
        drop(source);

        assert_eq!(
            raw.read_lbas.iter().filter(|read| **read == 5).count(),
            1,
            "body_lba 3 should be probed once, recovered by bulk, and served from the buffer"
        );
    }

    #[test]
    fn adjacent_probe_clean_block_is_buffered_without_extending_erasure_streak() {
        struct Collector(Mutex<Vec<RecoveryEvent>>);
        impl ParityAuditHook for Collector {
            fn on_recovery(&self, event: &RecoveryEvent) {
                self.0.lock().unwrap().push(event.clone());
            }
        }

        let scheme = bulk_scheme();
        let object_blocks = (1..=8).map(block).collect::<Vec<_>>();
        let sidecar = sidecar_for_object(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
        raw.medium_error_lbas.extend([2, 3, 5]); // body_lba 0, 1, 3.
        let collector = Arc::new(Collector(Mutex::new(Vec::new())));

        let mut source = ObjectParitySource::open(
            &mut raw,
            scheme,
            TAPE_UUID,
            scoped,
            BLOCK_SIZE,
            1,
            OpenTrust::RequireValidated,
        )
        .expect("object source opens");
        source.set_audit_hook(Some(collector.clone() as Arc<dyn ParityAuditHook>));

        let mut buf = vec![0u8; BLOCK_SIZE as usize];
        source
            .read_block(&mut buf)
            .expect("first erasure uses isolated recovery");
        assert_eq!(buf, object_blocks[0]);

        source
            .read_block(&mut buf)
            .expect("adjacent erasure probes and buffers clean next block");
        assert_eq!(buf, object_blocks[1]);
        assert_eq!(source.position().unwrap().lba, 2);

        source
            .read_block(&mut buf)
            .expect("clean probed block is served from the auto-read buffer");
        assert_eq!(buf, object_blocks[2]);
        assert_eq!(source.position().unwrap().lba, 3);

        source
            .read_block(&mut buf)
            .expect("clean buffered block breaks the erasure streak");
        assert_eq!(buf, object_blocks[3]);
        assert_eq!(source.position().unwrap().lba, 4);

        let events = collector.0.lock().unwrap();
        assert_eq!(
            events.len(),
            3,
            "clean probed blocks must not emit recovery audit events"
        );
        assert_eq!(events[0].at_requested, (1, 0));
        assert_eq!(events[1].at_requested, (1, 1));
        assert_eq!(events[2].at_requested, (1, 3));
        drop(events);
        drop(source);

        assert_eq!(
            raw.read_lbas.iter().filter(|read| **read == 4).count(),
            2,
            "body_lba 2 should be read once as a recovery peer, probed once, and not re-read when served from the buffer"
        );
        assert_eq!(
            raw.read_lbas.iter().filter(|read| **read == 6).count(),
            1,
            "serving a clean buffered block must not make the next erasure look adjacent and probe body_lba 4"
        );
    }

    #[test]
    fn adjacent_bulk_failure_falls_back_to_current_block_recovery() {
        struct Collector(Mutex<Vec<RecoveryEvent>>);
        impl ParityAuditHook for Collector {
            fn on_recovery(&self, event: &RecoveryEvent) {
                self.0.lock().unwrap().push(event.clone());
            }
        }

        let scheme = wide_bulk_scheme();
        let object_blocks = (1..=9).map(block).collect::<Vec<_>>();
        let sidecar = sidecar_for_object(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
        raw.medium_error_lbas.extend([
            2,  // body_lba 0, primes the adjacent-erasure detector.
            3,  // body_lba 1, current read: recoverable in isolation.
            4,  // body_lba 2, probed next read in a different stripe.
            7,  // body_lba 5, extra peer loss for body_lba 2's stripe.
            10, // body_lba 8, extra peer loss for body_lba 2's stripe.
        ]);
        let collector = Arc::new(Collector(Mutex::new(Vec::new())));

        let mut source = ObjectParitySource::open(
            &mut raw,
            scheme,
            TAPE_UUID,
            scoped,
            BLOCK_SIZE,
            1,
            OpenTrust::RequireValidated,
        )
        .expect("object source opens");
        source.set_audit_hook(Some(collector.clone() as Arc<dyn ParityAuditHook>));

        let mut buf = vec![0u8; BLOCK_SIZE as usize];
        source
            .read_block(&mut buf)
            .expect("first erasure primes adjacent recovery");
        assert_eq!(buf, object_blocks[0]);

        let bytes = source
            .read_block(&mut buf)
            .expect("current adjacent erasure falls back to isolated recovery");
        assert_eq!(bytes, BLOCK_SIZE as usize);
        assert_eq!(buf, object_blocks[1]);
        assert_eq!(source.position().unwrap().lba, 2);

        let events = collector.0.lock().unwrap();
        assert_eq!(
            events.len(),
            2,
            "failed opportunistic bulk escalation must not emit a false current-block failure"
        );
        assert!(events
            .iter()
            .all(|event| matches!(event.outcome, RecoveryOutcome::Recovered)));
        assert_eq!(events[0].at_requested, (1, 0));
        assert_eq!(events[1].at_requested, (1, 1));
        drop(events);
        drop(source);

        assert!(
            raw.read_lbas.contains(&7) && raw.read_lbas.contains(&10),
            "test fixture must exercise the failed two-block bulk attempt before fallback"
        );
    }

    #[test]
    fn object_source_non_erasure_sense_keys_propagate_without_recovery() {
        struct Collector(Mutex<Vec<RecoveryEvent>>);
        impl ParityAuditHook for Collector {
            fn on_recovery(&self, event: &RecoveryEvent) {
                self.0.lock().unwrap().push(event.clone());
            }
        }

        let cases = [
            ("not-ready medium-not-present", 0x02, 0x3A, 0x00),
            ("hardware data-path-failure", 0x04, 0x41, 0x00),
            ("hardware cartridge-fault", 0x04, 0x52, 0x00),
            ("illegal-request invalid-field-in-cdb", 0x05, 0x24, 0x00),
            ("data-protect write-protected", 0x07, 0x27, 0x00),
            ("aborted-command over-temperature", 0x0B, 0x0B, 0x01),
        ];

        for (label, key, asc, ascq) in cases {
            let scheme = scheme();
            let object_blocks = vec![block(1), block(2), block(3), block(4)];
            let sidecar = sidecar_for_object(&scheme, &object_blocks);
            let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
            let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
            raw.check_condition_lbas.push((4, key, asc, ascq)); // tape_file 1, body_lba 2.
            let collector = Arc::new(Collector(Mutex::new(Vec::new())));

            let mut source = ObjectParitySource::open(
                &mut raw,
                scheme,
                TAPE_UUID,
                scoped,
                BLOCK_SIZE,
                1,
                OpenTrust::RequireValidated,
            )
            .unwrap_or_else(|err| panic!("{label}: object source opens: {err}"));
            source.set_audit_hook(Some(collector.clone() as Arc<dyn ParityAuditHook>));

            source
                .locate(2)
                .unwrap_or_else(|err| panic!("{label}: locate failed body block: {err}"));
            let mut buf = vec![0u8; BLOCK_SIZE as usize];
            let err = source
                .read_block(&mut buf)
                .expect_err("non-erasure sense key must propagate");

            match err {
                TapeIoError::CheckCondition(ScsiError::CheckCondition { sense, .. }) => {
                    assert_eq!(sense[2] & 0x0F, key, "{label}: sense key");
                    assert_eq!(sense[12], asc, "{label}: ASC");
                    assert_eq!(sense[13], ascq, "{label}: ASCQ");
                }
                other => panic!("{label}: expected CHECK CONDITION propagation, got {other:?}"),
            }
            assert_eq!(
                source.position().unwrap().lba,
                2,
                "{label}: object cursor must not advance after propagated drive error"
            );
            drop(source);
            assert_eq!(
                raw.position().unwrap(),
                PhysicalPositionHint::new(4),
                "{label}: raw cursor remains at the failed physical read"
            );
            assert!(
                collector.0.lock().unwrap().is_empty(),
                "{label}: non-erasure drive-state errors must not emit recovery audit events"
            );
        }
    }

    #[test]
    fn object_source_forced_recovery_returns_block_and_audits() {
        struct Collector(Mutex<Vec<RecoveryEvent>>);
        impl ParityAuditHook for Collector {
            fn on_recovery(&self, event: &RecoveryEvent) {
                self.0.lock().unwrap().push(event.clone());
            }
        }

        let scheme = scheme();
        let object_blocks = vec![block(1), block(2), block(3), block(4)];
        let sidecar = sidecar_for_object(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
        let collector = Arc::new(Collector(Mutex::new(Vec::new())));

        let mut source = ObjectParitySource::open(
            &mut raw,
            scheme,
            TAPE_UUID,
            scoped,
            BLOCK_SIZE,
            1,
            OpenTrust::RequireValidated,
        )
        .expect("object source opens");
        source.set_audit_hook(Some(collector.clone() as Arc<dyn ParityAuditHook>));

        let repaired = source
            .recover_block_at(3)
            .expect("forced erasure recovery succeeds");

        assert_eq!(repaired, object_blocks[3]);
        assert_eq!(source.position().unwrap().lba, 4);
        let events = collector.0.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].outcome, RecoveryOutcome::Recovered));
        assert_eq!(events[0].at_requested, (1, 3));
    }

    #[test]
    fn sidecar_metadata_copy_loss_emits_health_audit_once() {
        #[derive(Default)]
        struct Collector {
            recovery: Mutex<Vec<RecoveryEvent>>,
            metadata: Mutex<Vec<SidecarMetadataHealthEvent>>,
        }

        impl ParityAuditHook for Collector {
            fn on_recovery(&self, event: &RecoveryEvent) {
                self.recovery.lock().unwrap().push(event.clone());
            }

            fn on_sidecar_metadata_health(&self, event: &SidecarMetadataHealthEvent) {
                self.metadata.lock().unwrap().push(event.clone());
            }
        }

        for expected_health in [
            SidecarMetadataHealth::TailCopyLost,
            SidecarMetadataHealth::PrimaryHeaderLost,
        ] {
            let scheme = scheme();
            let object_blocks = vec![block(1), block(2), block(3), block(4)];
            let sidecar = sidecar_for_object(&scheme, &object_blocks);
            let mut sidecar_blocks = sidecar.blocks.clone();
            match expected_health {
                SidecarMetadataHealth::TailCopyLost => {
                    let tail_start =
                        usize::try_from(sidecar.header.tail_header_start_block).unwrap();
                    sidecar_blocks[tail_start][0] ^= 0xFF;
                }
                SidecarMetadataHealth::PrimaryHeaderLost => {
                    sidecar_blocks[0][0] ^= 0xFF;
                }
                SidecarMetadataHealth::BothCopiesUsable => unreachable!("test covers loss cases"),
            }
            let scoped = scoped_map(sidecar_blocks.len() as u64, object_blocks.len() as u64);
            let mut raw = raw_tape(&object_blocks, &sidecar_blocks);
            let collector = Arc::new(Collector::default());

            let mut source = ObjectParitySource::open(
                &mut raw,
                scheme,
                TAPE_UUID,
                scoped,
                BLOCK_SIZE,
                1,
                OpenTrust::RequireValidated,
            )
            .expect("object source opens");
            source.set_audit_hook(Some(collector.clone() as Arc<dyn ParityAuditHook>));

            assert_eq!(
                source
                    .recover_block_at(3)
                    .expect("degraded sidecar metadata still recovers"),
                object_blocks[3]
            );
            assert_eq!(
                source
                    .recover_block_at(2)
                    .expect("same degraded sidecar remains recoverable"),
                object_blocks[2]
            );
            drop(source);

            let metadata = collector.metadata.lock().unwrap();
            assert_eq!(
                metadata.as_slice(),
                &[SidecarMetadataHealthEvent {
                    sidecar_tape_file_number: 2,
                    epoch_id: 0,
                    health: expected_health,
                }],
                "metadata health should be deduplicated for {expected_health:?}"
            );
            let recovery = collector.recovery.lock().unwrap();
            assert_eq!(recovery.len(), 2);
            assert!(recovery
                .iter()
                .all(|event| matches!(event.outcome, RecoveryOutcome::Recovered)));
        }
    }

    #[test]
    fn recover_region_returns_ordered_blocks_and_respects_memory_policy() {
        let scheme = bulk_scheme();
        let object_blocks = (1..=8).map(block).collect::<Vec<_>>();
        let sidecar = sidecar_for_object(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);

        let mut source = ObjectParitySource::open(
            &mut raw,
            scheme,
            TAPE_UUID,
            scoped,
            BLOCK_SIZE,
            1,
            OpenTrust::RequireValidated,
        )
        .expect("object source opens");

        let recovered = source
            .recover_region(
                0,
                4,
                BulkRecoveryPolicy {
                    max_recovery_cache_bytes: 8 * 1024,
                    allow_windowed_recovery: true,
                    max_stripes_per_window: 2,
                },
            )
            .expect("bulk region recovers");

        assert_eq!(recovered.start_body_lba, 0);
        assert_eq!(recovered.blocks, object_blocks[0..4].to_vec());

        let err = source
            .recover_region(
                0,
                4,
                BulkRecoveryPolicy {
                    max_recovery_cache_bytes: 3_000,
                    allow_windowed_recovery: false,
                    max_stripes_per_window: 1,
                },
            )
            .expect_err("non-windowed bulk recovery rejects a full plan over budget");
        match err {
            ParityError::RecoveryPlanExceedsMemoryBudget {
                needed_bytes,
                max_recovery_cache_bytes,
                allow_windowed_recovery,
            } => {
                assert_eq!(needed_bytes, 4_096);
                assert_eq!(max_recovery_cache_bytes, 3_000);
                assert!(!allow_windowed_recovery);
            }
            other => panic!("expected recovery memory-budget error, got {other:?}"),
        }

        let err = source
            .recover_region(
                0,
                4,
                BulkRecoveryPolicy {
                    max_recovery_cache_bytes: 1_023,
                    allow_windowed_recovery: true,
                    max_stripes_per_window: 8,
                },
            )
            .expect_err("returned region bytes alone can exceed the memory budget");
        match err {
            ParityError::RecoveryPlanExceedsMemoryBudget {
                needed_bytes,
                max_recovery_cache_bytes,
                allow_windowed_recovery,
            } => {
                assert_eq!(needed_bytes, 1_024);
                assert_eq!(max_recovery_cache_bytes, 1_023);
                assert!(allow_windowed_recovery);
            }
            other => panic!("expected recovery memory-budget error, got {other:?}"),
        }

        let windowed = source
            .recover_region(
                0,
                4,
                BulkRecoveryPolicy {
                    max_recovery_cache_bytes: 2_560,
                    allow_windowed_recovery: true,
                    max_stripes_per_window: 8,
                },
            )
            .expect("windowed budget derives a fitting one-stripe window from the byte cap");
        assert_eq!(windowed.blocks, object_blocks[0..4].to_vec());

        let err = source
            .recover_region(
                0,
                4,
                BulkRecoveryPolicy {
                    max_recovery_cache_bytes: 2_559,
                    allow_windowed_recovery: true,
                    max_stripes_per_window: 8,
                },
            )
            .expect_err("windowed bulk recovery still rejects if one window cannot fit");
        match err {
            ParityError::RecoveryPlanExceedsMemoryBudget {
                needed_bytes,
                max_recovery_cache_bytes,
                allow_windowed_recovery,
            } => {
                assert_eq!(needed_bytes, 2_560);
                assert_eq!(max_recovery_cache_bytes, 2_559);
                assert!(allow_windowed_recovery);
            }
            other => panic!("expected recovery memory-budget error, got {other:?}"),
        }

        let err = source
            .recover_region(
                8,
                1,
                BulkRecoveryPolicy {
                    max_recovery_cache_bytes: 4 * 1024,
                    allow_windowed_recovery: true,
                    max_stripes_per_window: 1,
                },
            )
            .expect_err("out-of-range bulk request is rejected");
        assert!(matches!(err, ParityError::FilemarkMapReconstruct(_)));

        let err = source
            .recover_region(
                0,
                1,
                BulkRecoveryPolicy {
                    max_recovery_cache_bytes: 0,
                    allow_windowed_recovery: true,
                    max_stripes_per_window: 1,
                },
            )
            .expect_err("zero-sized recovery cache is rejected");
        assert!(matches!(err, ParityError::Invariant(_)));

        let err = source
            .recover_region(
                0,
                1,
                BulkRecoveryPolicy {
                    max_recovery_cache_bytes: 4 * 1024,
                    allow_windowed_recovery: true,
                    max_stripes_per_window: 0,
                },
            )
            .expect_err("zero-stripe planner window is rejected");
        assert!(matches!(err, ParityError::Invariant(_)));
    }

    #[test]
    fn recover_ordinal_range_maps_ordinals_to_body_lbas() {
        let scheme = bulk_scheme();
        let object_blocks = (1..=8).map(block).collect::<Vec<_>>();
        let sidecar = sidecar_for_object(&scheme, &object_blocks);
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, 4, 0),
            TapeFileMapEntry::object(2, 4, 4),
            TapeFileMapEntry::parity_sidecar(3, sidecar.blocks.len() as u64, 0, 0, 8),
        ])
        .unwrap();
        let scoped = ScopedFilemarkMap::from_catalog(map, object_blocks.len() as u64);
        let mut records = Vec::new();
        records.push(Record::Block(vec![0xB0; BLOCK_SIZE as usize]));
        records.push(Record::Filemark);
        records.extend(object_blocks[0..4].iter().cloned().map(Record::Block));
        records.push(Record::Filemark);
        records.extend(object_blocks[4..8].iter().cloned().map(Record::Block));
        records.push(Record::Filemark);
        records.extend(sidecar.blocks.iter().cloned().map(Record::Block));
        records.push(Record::Filemark);
        let mut raw = RawVec::new(records);

        let mut source = ObjectParitySource::open(
            &mut raw,
            scheme,
            TAPE_UUID,
            scoped,
            BLOCK_SIZE,
            2,
            OpenTrust::RequireValidated,
        )
        .expect("object source opens");

        let recovered = source
            .recover_ordinal_range(
                4..8,
                BulkRecoveryPolicy {
                    max_recovery_cache_bytes: 8 * 1024,
                    allow_windowed_recovery: true,
                    max_stripes_per_window: 8,
                },
            )
            .expect("ordinal range recovers through bulk planner");

        assert_eq!(recovered.start_ordinal, 4);
        assert_eq!(recovered.blocks.len(), 4);
        for (offset, block) in recovered.blocks.iter().enumerate() {
            let body_lba = offset as u64;
            let ordinal = 4 + offset as u64;
            assert_eq!(block.ordinal, ordinal);
            assert_eq!(block.tape_file_number, 2);
            assert_eq!(block.body_lba, body_lba);
            assert_eq!(block.data, object_blocks[ordinal as usize]);
        }
        assert_eq!(source.position().unwrap().lba, 4);

        let empty = source
            .recover_ordinal_range(8..8, BulkRecoveryPolicy::default())
            .expect("empty range at object end is accepted");
        assert_eq!(empty.start_ordinal, 8);
        assert!(empty.blocks.is_empty());
    }

    #[test]
    fn recover_ordinal_range_rejects_ordinals_outside_current_object() {
        let object_a = [block(1), block(2)];
        let object_b = [block(3), block(4)];
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, 2, 0),
            TapeFileMapEntry::object(2, 2, 2),
        ])
        .unwrap();
        let scoped = ScopedFilemarkMap::from_catalog(map, 4);
        let mut records = Vec::new();
        records.push(Record::Block(vec![0xB0; BLOCK_SIZE as usize]));
        records.push(Record::Filemark);
        records.extend(object_a.iter().cloned().map(Record::Block));
        records.push(Record::Filemark);
        records.extend(object_b.iter().cloned().map(Record::Block));
        records.push(Record::Filemark);
        let mut raw = RawVec::new(records);

        let mut source = ObjectParitySource::open(
            &mut raw,
            scheme(),
            TAPE_UUID,
            scoped,
            BLOCK_SIZE,
            1,
            OpenTrust::RequireValidated,
        )
        .expect("object source opens");

        let descending_start = 2;
        let descending_end = 1;
        let err = source
            .recover_ordinal_range(2..3, BulkRecoveryPolicy::default())
            .expect_err("object-scoped source rejects another object's ordinal");
        assert!(matches!(err, ParityError::FilemarkMapReconstruct(_)));

        let err = source
            .recover_ordinal_range(
                descending_start..descending_end,
                BulkRecoveryPolicy::default(),
            )
            .expect_err("descending ordinal range is rejected");
        assert!(matches!(err, ParityError::FilemarkMapReconstruct(_)));
    }

    #[test]
    fn recover_region_deduplicates_peer_reads_per_epoch_plan() {
        let scheme = bulk_scheme();
        let object_blocks = (1..=8).map(block).collect::<Vec<_>>();
        let sidecar = sidecar_for_object(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);

        {
            let mut source = ObjectParitySource::open(
                &mut raw,
                scheme,
                TAPE_UUID,
                scoped.clone(),
                BLOCK_SIZE,
                1,
                OpenTrust::RequireValidated,
            )
            .expect("object source opens");

            let recovered = source
                .recover_region(
                    0,
                    4,
                    BulkRecoveryPolicy {
                        max_recovery_cache_bytes: 8 * 1024,
                        allow_windowed_recovery: true,
                        max_stripes_per_window: 8,
                    },
                )
                .expect("two-shard-per-stripe region recovers");
            assert_eq!(recovered.blocks, object_blocks[0..4].to_vec());
        }

        for ordinal in 4..8 {
            let position = scoped
                .map
                .position_for_ordinal(ordinal)
                .expect("peer ordinal is mapped");
            let lba = scoped
                .map
                .physical_position(position)
                .expect("peer ordinal has a physical position")
                .lba as usize;
            assert_eq!(
                raw.read_lbas.iter().filter(|read| **read == lba).count(),
                1,
                "data peer ordinal {ordinal} must be read once"
            );
        }

        let sidecar_header_blocks = sidecar.header.shard_index_block_count;
        for parity_entry_index in 0..sidecar.index.parity_entries.len() {
            let lba = scoped
                .map
                .physical_position(TapeFilePosition {
                    tape_file_number: 2,
                    block_within_file: u64::from(sidecar_header_blocks) + parity_entry_index as u64,
                })
                .expect("parity shard has a physical position")
                .lba as usize;
            assert_eq!(
                raw.read_lbas.iter().filter(|read| **read == lba).count(),
                1,
                "parity peer entry {parity_entry_index} must be read once"
            );
        }
    }

    #[test]
    fn recover_region_fails_when_requested_interval_exceeds_stripe_tolerance() {
        let scheme = bulk_scheme();
        let object_blocks = (1..=8).map(block).collect::<Vec<_>>();
        let sidecar = sidecar_for_object(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);

        let mut source = ObjectParitySource::open(
            &mut raw,
            scheme,
            TAPE_UUID,
            scoped,
            BLOCK_SIZE,
            1,
            OpenTrust::RequireValidated,
        )
        .expect("object source opens");

        let err = source
            .recover_region(
                0,
                5,
                BulkRecoveryPolicy {
                    max_recovery_cache_bytes: 8 * 1024,
                    allow_windowed_recovery: true,
                    max_stripes_per_window: 8,
                },
            )
            .expect_err("three requested shards in one stripe exceed m=2");
        match err {
            ParityError::Unrecoverable {
                lost_count, limit, ..
            } => {
                assert_eq!(lost_count, 3);
                assert_eq!(limit, 2);
            }
            other => panic!("expected unrecoverable stripe, got {other:?}"),
        }
    }

    #[test]
    #[ignore = "allocates real 64 MiB recovery payloads for the 11.13b media-scale gate"]
    fn recover_region_media_scale_64m_windowed_peer_dedup() {
        run_media_scale_recover_region_success(64, 32, 8);
    }

    #[test]
    #[ignore = "allocates real 512 MiB recovery payloads for the 11.13b media-scale gate"]
    fn recover_region_media_scale_512m_windowed_peer_dedup() {
        run_media_scale_recover_region_success(512, 256, 64);
    }

    #[test]
    #[ignore = "allocates real 513 MiB recovery payloads for the 11.13b partial-failure gate"]
    fn recover_region_media_scale_513m_partial_fails() {
        let block_size = MIB as u32;
        let block_count = 513;
        let scheme = media_scale_scheme(256);
        let object_blocks = media_scale_object_blocks(&scheme, block_size);
        let sidecar = sidecar_for_object_with_block_size(&scheme, &object_blocks, block_size);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let mut raw = RawBorrowedTape::new(&object_blocks, &sidecar.blocks);

        let mut source = ObjectParitySource::open(
            &mut raw,
            scheme.clone(),
            TAPE_UUID,
            scoped,
            block_size,
            1,
            OpenTrust::RequireValidated,
        )
        .expect("media-scale object source opens");

        let err = source
            .recover_region(
                0,
                block_count,
                media_scale_policy(
                    block_count,
                    block_size,
                    &scheme,
                    scheme.stripes_per_neighborhood,
                ),
            )
            .expect_err("513 MiB contiguous region exceeds the S*m tolerance by one block");
        match err {
            ParityError::Unrecoverable {
                lost_count, limit, ..
            } => {
                assert_eq!(lost_count, 3);
                assert_eq!(limit, 2);
            }
            other => panic!("expected media-scale unrecoverable stripe, got {other:?}"),
        }
    }

    #[test]
    fn unvalidated_suffix_can_read_but_not_recover() {
        let object_a = [block(1), block(2)];
        let object_b = [block(3), block(4)];
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, 2, 0),
            TapeFileMapEntry::object(2, 2, 2),
        ])
        .unwrap();
        let scoped = ScopedFilemarkMap {
            map,
            validated_prefix_tape_files: Some(2),
            scope: MapScope::Prefix {
                map_total_data_ordinals: 2,
                highest_protected_ordinal: 2,
            },
        };
        let mut records = Vec::new();
        records.push(Record::Block(vec![0xB0; BLOCK_SIZE as usize]));
        records.push(Record::Filemark);
        records.extend(object_a.iter().cloned().map(Record::Block));
        records.push(Record::Filemark);
        records.extend(object_b.iter().cloned().map(Record::Block));
        records.push(Record::Filemark);
        let mut raw = RawVec::new(records);

        let rejected = match ObjectParitySource::open(
            &mut raw,
            scheme(),
            TAPE_UUID,
            scoped.clone(),
            BLOCK_SIZE,
            2,
            OpenTrust::RequireValidated,
        ) {
            Err(err) => err,
            Ok(_) => panic!("require-validated rejects suffix object"),
        };
        assert!(matches!(
            rejected,
            ParityError::OutsideValidatedMapPrefix {
                ordinal: 2,
                prefix_ordinals: 2
            }
        ));
        assert_eq!(raw.configured_block_size, None);

        let mut source = ObjectParitySource::open(
            &mut raw,
            scheme(),
            TAPE_UUID,
            scoped,
            BLOCK_SIZE,
            2,
            OpenTrust::AllowTarOnlyUnverified,
        )
        .expect("tar-only suffix open succeeds");
        assert!(source.is_tar_only_unverified());
        let mut buf = vec![0u8; BLOCK_SIZE as usize];
        source.read_block(&mut buf).expect("clean tar-only read");
        assert_eq!(buf, object_b[0]);

        let err = source
            .recover_block_at(0)
            .expect_err("suffix recovery remains refused");
        assert!(matches!(
            err,
            ParityError::OutsideValidatedMapPrefix {
                ordinal: 2,
                prefix_ordinals: 2
            }
        ));
    }
}
