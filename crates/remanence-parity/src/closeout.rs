//! Shared incremental parity for original writes and committed-range readback.
//!
//! Only parity buffers and ordered whole-block CRCs survive a push. Callers
//! reserve memory before construction and retain that permit through emission.

pub use crate::check_value::{CheckValue, CheckValueBuilder};
use crate::codec::ReedSolomonCodec;
use crate::sidecar::{data_shard_crc64, EncodedSidecarTapeFile, SidecarDescriptor};
use crate::{ParityError, ParityScheme};
use remanence_library::TapeIoError;

/// One committed Object's physical extent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommittedExtent {
    /// Dense tape-file number.
    pub tape_file_number: u64,
    /// First Object data ordinal.
    pub first_ordinal: u64,
    /// Number of data blocks.
    pub block_count: u64,
    /// Barrier-proved first-block LBA.
    pub physical_start_lba: u64,
}

/// The committed range whose sidecar is still owed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UncoveredRange {
    /// Monotonic epoch label.
    pub epoch_id: u64,
    /// First uncovered ordinal.
    pub w: u64,
    /// End-exclusive committed ordinal.
    pub t: u64,
    /// Ordered committed Object extents.
    pub extents: Vec<CommittedExtent>,
    /// Original-write check from the durable commit.
    pub check: CheckValue,
}

/// One epoch's parity and ordered CRCs; never owns data blocks.
pub struct EpochAccumulator {
    pub(crate) tape_uuid: [u8; 16],
    pub(crate) scheme: ParityScheme,
    pub(crate) block_size: u32,
    pub(crate) epoch_id: u64,
    pub(crate) epoch_start: u64,
    pub(crate) next_ordinal: u64,
    pub(crate) codec: ReedSolomonCodec,
    pub(crate) parity_accumulators: Vec<Vec<Vec<u8>>>,
    pub(crate) current_epoch_data_crc64s: Vec<u64>,
    check: CheckValueBuilder,
}

impl EpochAccumulator {
    /// Core payload: `S*m*B + 8*S*k`, checked before allocation.
    /// Containers, read batches and encoding scratch must be reserved separately.
    pub fn reservation_bytes(scheme: &ParityScheme, block_size: u32) -> Result<u64, ParityError> {
        let s = u64::from(scheme.stripes_per_neighborhood);
        s.checked_mul(u64::from(scheme.parity_blocks_per_stripe))
            .and_then(|v| v.checked_mul(u64::from(block_size)))
            .and_then(|v| {
                s.checked_mul(u64::from(scheme.data_blocks_per_stripe))
                    .and_then(|n| n.checked_mul(8))
                    .and_then(|n| v.checked_add(n))
            })
            .ok_or(ParityError::Invariant("epoch reservation overflows"))
    }

    /// Conservative closeout working set, including containers, codec, bounded
    /// forward reads, index encoding, both index copies, and extent summaries.
    pub fn working_reservation_bytes(
        scheme: &ParityScheme,
        block_size: u32,
        extent_count: u64,
    ) -> Result<u64, ParityError> {
        scheme.validate()?;
        let core = Self::reservation_bytes(scheme, block_size)?;
        let s = u64::from(scheme.stripes_per_neighborhood);
        let m = u64::from(scheme.parity_blocks_per_stripe);
        let k = u64::from(scheme.data_blocks_per_stripe);
        let p = s
            .checked_mul(m)
            .ok_or(ParityError::Invariant("closeout parity count overflows"))?;
        let d = s
            .checked_mul(k)
            .ok_or(ParityError::Invariant("closeout data count overflows"))?;
        let h = crate::sidecar::checked_sidecar_index_capacity_layout(u64::from(block_size), p, d)?
            .block_count;
        let bytes = (|| {
            let headers = s
                .checked_add(p.checked_mul(4)?)?
                .checked_add(m)?
                .checked_mul(std::mem::size_of::<Vec<u8>>() as u64)?;
            let index = p
                .checked_mul(std::mem::size_of::<crate::sidecar::ParityShardIndexEntry>() as u64)?
                .checked_add(d.checked_mul(8)?)?
                .checked_mul(6)?;
            let scratch = h
                .checked_mul(3)?
                .checked_add(3)?
                .checked_add(u64::from(remanence_library::DEFAULT_TAPE_IO_BATCH_BLOCKS))?
                .checked_mul(u64::from(block_size))?;
            let extents = extent_count
                .checked_mul(std::mem::size_of::<CommittedExtent>() as u64)?
                .checked_mul(3)?;
            core.checked_add(headers)?
                .checked_add(m.checked_mul(k)?)?
                .checked_add(index)?
                .checked_add(scratch)?
                .checked_add(extents)?
                .checked_add(std::mem::size_of::<Self>() as u64)
        })();
        bytes.ok_or(ParityError::Invariant(
            "closeout working reservation overflows",
        ))
    }

    /// Start an empty epoch after its memory has been reserved.
    pub fn new(
        tape_uuid: [u8; 16],
        scheme: &ParityScheme,
        block_size: u32,
        epoch_id: u64,
        epoch_start: u64,
    ) -> Result<Self, ParityError> {
        scheme.validate()?;
        if block_size == 0 {
            return Err(ParityError::Invariant("epoch block size is zero"));
        }
        Self::reservation_bytes(scheme, block_size)?;
        let codec = ReedSolomonCodec::new(scheme)?;
        let capacity = usize::try_from(
            u64::from(scheme.stripes_per_neighborhood) * u64::from(scheme.data_blocks_per_stripe),
        )
        .map_err(|_| ParityError::Invariant("epoch CRC count exceeds usize"))?;
        let parity_accumulators = (0..scheme.stripes_per_neighborhood)
            .map(|_| codec.new_parity_accumulators(block_size as usize))
            .collect();
        Ok(Self {
            tape_uuid,
            scheme: scheme.clone(),
            block_size,
            epoch_id,
            epoch_start,
            next_ordinal: epoch_start,
            codec,
            parity_accumulators,
            current_epoch_data_crc64s: Vec::with_capacity(capacity),
            check: CheckValueBuilder::new(),
        })
    }

    /// Adopt rebuilt parity without allocating another epoch payload.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_rebuilt(
        tape_uuid: [u8; 16],
        scheme: &ParityScheme,
        block_size: u32,
        epoch_id: u64,
        epoch_start: u64,
        next_ordinal: u64,
        parity_accumulators: Vec<Vec<Vec<u8>>>,
        block_crcs: Vec<u64>,
    ) -> Result<Self, ParityError> {
        scheme.validate()?;
        let count = next_ordinal
            .checked_sub(epoch_start)
            .ok_or(ParityError::Invariant("rebuilt epoch range reversed"))?;
        if block_size == 0
            || count != block_crcs.len() as u64
            || count
                > u64::from(scheme.stripes_per_neighborhood)
                    * u64::from(scheme.data_blocks_per_stripe)
            || parity_accumulators.len() != scheme.stripes_per_neighborhood as usize
            || parity_accumulators.iter().any(|stripe| {
                stripe.len() != scheme.parity_blocks_per_stripe as usize
                    || stripe
                        .iter()
                        .any(|shard| shard.len() != block_size as usize)
            })
        {
            return Err(ParityError::Invariant("rebuilt epoch geometry mismatch"));
        }
        let mut check = CheckValueBuilder::new();
        for &crc in &block_crcs {
            check.push(crc);
        }
        Ok(Self {
            tape_uuid,
            scheme: scheme.clone(),
            block_size,
            epoch_id,
            epoch_start,
            next_ordinal,
            codec: ReedSolomonCodec::new(scheme)?,
            parity_accumulators,
            current_epoch_data_crc64s: block_crcs,
            check,
        })
    }

    /// Accumulate a full block at exactly the next ordinal; reject gaps and repeats.
    pub fn push_block(&mut self, ordinal: u64, block: &[u8]) -> Result<(), ParityError> {
        if ordinal != self.next_ordinal {
            return Err(ParityError::Invariant("epoch ordinal is not consecutive"));
        }
        let next = ordinal
            .checked_add(1)
            .ok_or(ParityError::Invariant("epoch ordinal overflows"))?;
        let offset = ordinal - self.epoch_start;
        let s = u64::from(self.scheme.stripes_per_neighborhood);
        if offset >= s * u64::from(self.scheme.data_blocks_per_stripe)
            || block.len() != self.block_size as usize
        {
            return Err(ParityError::Invariant("epoch block exceeds geometry"));
        }
        let accumulators = self
            .parity_accumulators
            .get_mut((offset % s) as usize)
            .ok_or(ParityError::Invariant(
                "epoch parity accumulators unavailable",
            ))?;
        self.codec
            .accumulate((offset / s) as usize, block, accumulators)?;
        let crc = data_shard_crc64(block);
        self.current_epoch_data_crc64s.push(crc);
        self.check.push(crc);
        self.next_ordinal = next;
        Ok(())
    }

    /// Rebase an unused accumulator without allocating a second parity payload.
    pub(crate) fn restart_empty(
        &mut self,
        epoch_id: u64,
        epoch_start: u64,
    ) -> Result<(), ParityError> {
        if !self.current_epoch_data_crc64s.is_empty() {
            return Err(ParityError::Invariant("cannot rebase a populated epoch"));
        }
        self.epoch_id = epoch_id;
        self.epoch_start = epoch_start;
        self.next_ordinal = epoch_start;
        self.check = CheckValueBuilder::new();
        Ok(())
    }

    /// Transfer parity ownership to a queued sidecar; the sink resets afterward.
    pub(crate) fn take_parity_shards(
        &mut self,
        block_size: usize,
    ) -> Result<Vec<Vec<u8>>, ParityError> {
        if block_size != self.block_size as usize
            || self.parity_accumulators.len() != self.scheme.stripes_per_neighborhood as usize
            || self.parity_accumulators.iter().any(|stripe| {
                stripe.len() != self.codec.parity_blocks()
                    || stripe.iter().any(|shard| shard.len() != block_size)
            })
        {
            // A failed transfer invalidates the whole accumulator, including
            // stripes whose shape happened to be valid. Further pushes fail.
            self.parity_accumulators.clear();
            return Err(ParityError::Invariant("epoch parity shape mismatch"));
        }
        Ok(std::mem::take(&mut self.parity_accumulators)
            .into_iter()
            .flatten()
            .collect())
    }

    /// End-exclusive ordinal accumulated so far.
    pub fn covered_end(&self) -> u64 {
        self.next_ordinal
    }

    /// Check of the ordered original block CRCs.
    pub fn check_value(&self) -> CheckValue {
        self.check.value()
    }

    /// Consume the parity buffers into the sidecar's physical block order.
    pub fn into_sidecar(self) -> Result<EncodedSidecarTapeFile, ParityError> {
        let descriptor = SidecarDescriptor {
            tape_uuid: self.tape_uuid,
            epoch_id: self.epoch_id,
            k: self.scheme.data_blocks_per_stripe,
            m: self.scheme.parity_blocks_per_stripe,
            stripes_per_epoch: self.scheme.stripes_per_neighborhood,
            block_size: self.block_size,
            protected_ordinal_start: self.epoch_start,
            protected_ordinal_end_exclusive: self.next_ordinal,
        };
        crate::sidecar::encode_sidecar_owned(
            &descriptor,
            self.parity_accumulators.into_iter().flatten().collect(),
            self.current_epoch_data_crc64s,
        )
    }
}

/// Positioned output for a checked sidecar, implemented by the owning adapter.
pub trait SidecarOut {
    /// Emit a complete fixed-size block.
    fn write_block(&mut self, block: &[u8]) -> Result<(), TapeIoError>;
    /// Terminate the sidecar file.
    fn write_filemark(&mut self) -> Result<remanence_library::WriteFilemarksOutcome, TapeIoError>;
}

/// Adapter for reading committed extents; boundary failures remain typed.
pub trait CommittedReader {
    /// Prove the same tape identity again before a second read pass.
    fn reprove_identity(&mut self) -> Result<(), CloseoutError>;
    /// Locate a committed physical coordinate.
    fn locate(&mut self, partition: u32, lba: u64) -> Result<(), CloseoutError>;
    /// Fill exactly one fixed block; reject boundaries and short records.
    fn read_block(&mut self, buf: &mut [u8]) -> Result<(), CloseoutError>;
    /// Read one contiguous committed extent. Adapters may batch these reads.
    fn read_extent(
        &mut self,
        partition: u32,
        lba: u64,
        start: u64,
        end: u64,
        accumulator: &mut EpochAccumulator,
    ) -> Result<(), CloseoutError> {
        self.locate(partition, lba)?;
        let mut block = vec![0; accumulator.block_size as usize];
        for ordinal in start..end {
            self.read_block(&mut block)?;
            accumulator.push_block(ordinal, &block)?;
        }
        Ok(())
    }
}

/// Failure to establish checked rebuilt parity.
#[derive(Debug, thiserror::Error)]
pub enum CloseoutError {
    /// Both full-range reads disagree with the original-write check.
    #[error("closeout check mismatch: expected {expected:?}, found {found:?}")]
    CheckMismatch {
        /// Durable original-write value.
        expected: CheckValue,
        /// Value from the second read.
        found: CheckValue,
    },
    /// Committed extent contradicts a boundary or length observed on tape.
    #[error("closeout committed boundary contradiction at LBA {lba}")]
    Boundary {
        /// Physical coordinate of the contradiction.
        lba: u64,
    },
    /// Medium or transport failure.
    #[error(transparent)]
    TapeIo(#[from] TapeIoError),
    /// Reservation refused before tape work.
    #[error("closeout memory reservation refused")]
    Memory,
    /// Invalid geometry or range.
    #[error(transparent)]
    Parity(#[from] ParityError),
}

/// Read an exactly covered committed range, retrying a check mismatch once.
/// The caller holds its memory permit before entry and persists a fence on a
/// returned check mismatch. The first accumulator is dropped before reproof.
pub fn rebuild_uncovered(
    tape_uuid: [u8; 16],
    reader: &mut dyn CommittedReader,
    scheme: &ParityScheme,
    block_size: u32,
    partition: u32,
    range: &UncoveredRange,
) -> Result<EpochAccumulator, CloseoutError> {
    for attempt in 0..2 {
        let accumulator =
            rebuild_uncovered_once(tape_uuid, reader, scheme, block_size, partition, range)?;
        let found = accumulator.check_value();
        if found == range.check {
            return Ok(accumulator);
        }
        // Release the whole parity payload before identity proof or another allocation.
        drop(accumulator);
        if attempt == 1 {
            return Err(CloseoutError::CheckMismatch {
                expected: range.check,
                found,
            });
        }
        tracing::warn!(
            epoch_id = range.epoch_id,
            start = range.w,
            end = range.t,
            "closeout check mismatch; re-proving identity before one full reread"
        );
        reader.reprove_identity()?;
    }
    unreachable!("two attempts return")
}

/// One shared read pass, also used for pre-cut-over UnverifiedRebuild authority.
/// Only the checked entry point may claim agreement with original-write bytes.
pub(crate) fn rebuild_uncovered_once(
    tape_uuid: [u8; 16],
    reader: &mut dyn CommittedReader,
    scheme: &ParityScheme,
    block_size: u32,
    partition: u32,
    range: &UncoveredRange,
) -> Result<EpochAccumulator, CloseoutError> {
    let count = range
        .t
        .checked_sub(range.w)
        .ok_or(ParityError::Invariant("uncovered range reversed"))?;
    if count > u64::from(scheme.stripes_per_neighborhood) * u64::from(scheme.data_blocks_per_stripe)
    {
        return Err(ParityError::Invariant("uncovered range exceeds epoch").into());
    }
    let mut spans = Vec::new();
    let mut previous_file = None;
    let mut next = range.w;
    for extent in &range.extents {
        let end = extent
            .first_ordinal
            .checked_add(extent.block_count)
            .ok_or(ParityError::Invariant("committed extent overflows"))?
            .min(range.t);
        let start = extent.first_ordinal.max(range.w);
        if start >= end {
            continue;
        }
        if previous_file.is_some_and(|file| file >= extent.tape_file_number) {
            return Err(ParityError::Invariant("committed files out of order").into());
        }
        previous_file = Some(extent.tape_file_number);
        if start != next {
            return Err(ParityError::Invariant("committed extent gap or overlap").into());
        }
        let lba = extent
            .physical_start_lba
            .checked_add(start - extent.first_ordinal)
            .ok_or(ParityError::Invariant("committed LBA overflows"))?;
        lba.checked_add(end - start)
            .ok_or(ParityError::Invariant("committed end LBA overflows"))?;
        spans.push((start, end, lba));
        next = end;
    }
    if next != range.t {
        return Err(ParityError::Invariant("committed extent gap").into());
    }
    let mut accumulator =
        EpochAccumulator::new(tape_uuid, scheme, block_size, range.epoch_id, range.w)?;
    for (start, end, lba) in spans {
        reader.read_extent(partition, lba, start, end, &mut accumulator)?;
    }
    Ok(accumulator)
}

impl CloseoutError {
    /// Preserve existing reader classes at the legacy parity API boundary.
    pub(crate) fn into_parity(self) -> ParityError {
        match self {
            Self::CheckMismatch { expected, found } => {
                ParityError::CloseoutCheckMismatch { expected, found }
            }
            Self::Boundary { lba } => {
                ParityError::ResumeAppend(format!("committed boundary contradiction at LBA {lba}"))
            }
            Self::TapeIo(error) => ParityError::TapeIo(error),
            Self::Parity(error) => error,
            Self::Memory => ParityError::Invariant("closeout memory reservation refused"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scheme() -> ParityScheme {
        ParityScheme {
            stripes_per_neighborhood: 3,
            data_blocks_per_stripe: 4,
            parity_blocks_per_stripe: 2,
            ..crate::default_scheme()
        }
    }

    #[test]
    fn checked_core_reservation_default_and_overflow() {
        assert_eq!(
            EpochAccumulator::reservation_bytes(&crate::default_scheme(), 262_144).unwrap(),
            537_395_200
        );
        let mut scheme = crate::default_scheme();
        scheme.stripes_per_neighborhood = u32::MAX;
        scheme.parity_blocks_per_stripe = u16::MAX;
        assert!(EpochAccumulator::reservation_bytes(&scheme, u32::MAX).is_err());
    }

    #[test]
    fn wrong_shard_shape_invalidates_all_stripes() {
        for wrong_block_size in [false, true] {
            let mut epoch = EpochAccumulator::new([7; 16], &scheme(), 256, 2, 10).unwrap();
            epoch.push_block(10, &[1; 256]).unwrap();
            if !wrong_block_size {
                // The next ordinal would use stripe 1, which is still valid.
                epoch.parity_accumulators[0][0].pop();
            }
            let check = epoch.check_value();
            assert!(matches!(
                epoch.take_parity_shards(if wrong_block_size { 255 } else { 256 }),
                Err(ParityError::Invariant(_))
            ));
            assert!(matches!(
                epoch.push_block(11, &[2; 256]),
                Err(ParityError::Invariant(_))
            ));
            assert!(matches!(
                epoch.take_parity_shards(256),
                Err(ParityError::Invariant(_))
            ));
            assert_eq!(epoch.check_value(), check);
            assert_eq!(epoch.covered_end(), 11);
        }
    }

    #[test]
    fn ordinal_gaps_repeats_and_overrun_leave_check_unchanged() {
        let mut epoch = EpochAccumulator::new([7; 16], &scheme(), 256, 2, 10).unwrap();
        assert!(epoch.push_block(11, &[1; 256]).is_err());
        assert_eq!(epoch.check_value(), CheckValue::EMPTY);
        epoch.push_block(10, &[1; 256]).unwrap();
        let check = epoch.check_value();
        assert!(epoch.push_block(10, &[2; 256]).is_err());
        assert!(epoch.push_block(12, &[2; 256]).is_err());
        assert!(epoch.push_block(11, &[2; 255]).is_err());
        assert_eq!(epoch.check_value(), check);
        for ordinal in 11..22 {
            epoch.push_block(ordinal, &[2; 256]).unwrap();
        }
        assert!(epoch.push_block(22, &[2; 256]).is_err());
        assert_eq!(epoch.covered_end(), 22);
    }

    #[test]
    fn shared_check_vectors_and_pattern_blocks() {
        let vectors: serde_json::Value = serde_json::from_str(include_str!(
            "../../../fixtures/rem-writer-check-value/vectors.json"
        ))
        .unwrap();
        let cases = vectors["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 4);
        for case in cases {
            let mut check = CheckValueBuilder::new();
            for crc in case["block_crcs"].as_array().unwrap() {
                check.push(u64::from_str_radix(crc.as_str().unwrap(), 16).unwrap());
            }
            let expected =
                CheckValue(u64::from_str_radix(case["expected"].as_str().unwrap(), 16).unwrap());
            assert_eq!(check.value(), expected);
            if let Some(pattern) = case.get("block_pattern") {
                let mut epoch = EpochAccumulator::new([7; 16], &scheme(), 262_144, 3, 19).unwrap();
                for i in 0..pattern["block_count"].as_u64().unwrap() {
                    let block: Vec<u8> = (0..262_144).map(|j| ((i * 31 + j) % 251) as u8).collect();
                    epoch.push_block(19 + i, &block).unwrap();
                }
                assert_eq!(epoch.check_value(), expected);
            }
        }
    }

    #[test]
    fn encoding_moves_parity_and_matches_borrowed_encoder_short_and_full() {
        for count in [5, 12] {
            let mut epoch = EpochAccumulator::new([7; 16], &scheme(), 256, 2, 10).unwrap();
            for i in 0..count {
                epoch.push_block(10 + i, &[i as u8; 256]).unwrap();
            }
            let pointers: Vec<_> = epoch
                .parity_accumulators
                .iter()
                .flatten()
                .map(|b| b.as_ptr())
                .collect();
            let descriptor = SidecarDescriptor {
                tape_uuid: [7; 16],
                epoch_id: 2,
                k: 4,
                m: 2,
                stripes_per_epoch: 3,
                block_size: 256,
                protected_ordinal_start: 10,
                protected_ordinal_end_exclusive: 10 + count,
            };
            let borrowed: Vec<_> = epoch.parity_accumulators.iter().flatten().collect();
            let expected = crate::sidecar::encode_sidecar_tape_file(
                &descriptor,
                &borrowed,
                epoch.current_epoch_data_crc64s.clone(),
            )
            .unwrap();
            let actual = epoch.into_sidecar().unwrap();
            assert_eq!(actual, expected);
            for pointer in pointers {
                assert!(actual.blocks.iter().any(|b| b.as_ptr() == pointer));
            }
        }
    }
}
