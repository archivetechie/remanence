//! Bounded observations for the protected pass. Only data READs move; all
//! metadata, comparison and finding construction remain in the verifier.

use super::{BlockFailureReason, ParityError, Probe, ScopedFilemarkMap, SequentialReader};
use crate::filemark_map::{TapeFileKind, TapeFileMapEntry, TapeFilePosition};
use crate::sidecar::data_shard_crc64;
use std::mem::size_of;

pub(super) const PENDING_BUDGET: usize = 64 * 1024 * 1024;

/// Stored as parallel byte and u64 arrays, not padded Rust enum records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DataObservation {
    tag: u8,
    payload: u64,
}

impl DataObservation {
    pub(super) fn from_probe(probe: Probe) -> Self {
        let (tag, payload) = match probe {
            Probe::Block(block) => (0, data_shard_crc64(&block)),
            Probe::WrongLength(length) => (1, length),
            Probe::Boundary("filemark where a block belongs") => (2, 0),
            Probe::Boundary(_) => (3, 0),
            Probe::Unreadable => (4, 0),
        };
        Self { tag, payload }
    }

    pub(super) fn crc(self) -> Option<u64> {
        (self.tag == 0).then_some(self.payload)
    }

    pub(super) fn failure(self) -> Option<BlockFailureReason> {
        match self.tag {
            0 => None,
            1 => Some(BlockFailureReason::WrongLength {
                measured_bytes: self.payload,
            }),
            2 | 3 => Some(BlockFailureReason::UnexpectedBoundary),
            4 => Some(BlockFailureReason::Unreadable),
            _ => unreachable!("observation tag is constructed internally"),
        }
    }
}

/// Exactly the original data loop's predicate, including suffix Objects.
pub(super) fn overlap(
    entry: &TapeFileMapEntry,
    start: u64,
    end: u64,
) -> Result<Option<(u64, u64, u64)>, ParityError> {
    if entry.kind != TapeFileKind::Object {
        return Ok(None);
    }
    let Some(first) = entry.first_parity_data_ordinal else {
        return Ok(None);
    };
    let object_end = first.checked_add(entry.block_count).ok_or_else(|| {
        ParityError::FilemarkMapReconstruct(format!(
            "object tape file {} ordinal range overflows",
            entry.tape_file_number
        ))
    })?;
    let lo = first.max(start);
    let hi = object_end.min(end);
    Ok((lo < hi).then_some((first, lo, hi)))
}

/// One complete overlap run. Boxed slices give exact capacity and never grow.
/// The link and all addressing/storage descriptors are charged before allocation.
pub(super) struct PendingRun {
    pub(super) file: u64,
    tags: Box<[u8]>,
    payloads: Box<[u64]>,
    next: Option<Box<PendingRun>>,
}

impl PendingRun {
    pub(super) fn len(&self) -> usize {
        self.tags.len()
    }
    pub(super) fn get(&self, offset: usize) -> DataObservation {
        DataObservation {
            tag: self.tags[offset],
            payload: self.payloads[offset],
        }
    }
    fn charge(&self) -> usize {
        size_of::<Self>() + self.len() * 9
    }
}

pub(super) struct PendingEpoch {
    file: u64,
    start: u64,
    end: u64,
    stopped: bool,
    runs: Option<Box<PendingRun>>,
}

impl PendingEpoch {
    pub(super) fn runs(&self) -> impl Iterator<Item = &PendingRun> {
        std::iter::successors(self.runs.as_deref(), |run| run.next.as_deref())
    }

    #[cfg(test)]
    pub(super) fn run(&self, file: u64) -> Option<&PendingRun> {
        self.runs().find(|run| run.file == file)
    }

    // Iterative destruction also bounds stack use on a map with many Objects.
    fn clear(&mut self) -> (usize, usize) {
        let mut total = 0;
        let mut payload = 0;
        while let Some(mut run) = self.runs.take() {
            total += run.charge();
            payload += run.len() * 9;
            self.runs = run.next.take();
        }
        (total, payload)
    }
}

impl Drop for PendingEpoch {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Global pending-storage accounting includes the entire epoch table capacity,
/// even after individual epochs release their runs. Indexes/findings are separate.
#[derive(Default)]
pub(super) struct ForwardPass {
    epochs: Box<[PendingEpoch]>,
    budget: usize,
    pub(super) used: usize,
    pub(super) payload: usize,
    pub(super) high_water: usize,
    pub(super) payload_high_water: usize,
}

impl ForwardPass {
    pub(super) fn plan(map: &ScopedFilemarkMap, budget: usize) -> Result<Self, ParityError> {
        let selected = |e: &&TapeFileMapEntry| {
            e.kind == TapeFileKind::ParitySidecar && map.is_validated(e.tape_file_number)
        };
        let count = map.map.entries().iter().filter(selected).count();
        let Some(charge) = count
            .checked_mul(size_of::<PendingEpoch>())
            .filter(|&n| n <= budget)
        else {
            return Ok(Self::default());
        };
        let mut epochs = Vec::with_capacity(count);
        for sidecar in map.map.entries().iter().filter(selected) {
            let (Some(start), Some(end)) = (
                sidecar.protected_ordinal_start,
                sidecar.protected_ordinal_end_exclusive,
            ) else {
                // The original sidecar verifier diagnoses incomplete metadata.
                continue;
            };
            let mut stopped = start >= end;
            for other in map.map.entries() {
                if overlap(other, start, end)?.is_some()
                    && other.tape_file_number >= sidecar.tape_file_number
                {
                    stopped = true;
                }
                if other.kind == TapeFileKind::ParitySidecar
                    && other.tape_file_number != sidecar.tape_file_number
                {
                    if let (Some(lo), Some(hi)) = (
                        other.protected_ordinal_start,
                        other.protected_ordinal_end_exclusive,
                    ) {
                        stopped |= lo < end && start < hi;
                    }
                }
            }
            epochs.push(PendingEpoch {
                file: sidecar.tape_file_number,
                start,
                end,
                stopped,
                runs: None,
            });
        }
        // Tape-file order need not agree with sidecar range order. Object reads
        // must still proceed by block offset, including adjacent epoch ranges.
        epochs.sort_unstable_by_key(|epoch| epoch.start);
        let epochs = epochs.into_boxed_slice();
        let used = epochs.len() * size_of::<PendingEpoch>();
        Ok(Self {
            epochs,
            budget,
            used,
            high_water: charge,
            ..Self::default()
        })
    }

    #[cfg(test)]
    pub(super) fn allocated_bytes(&self) -> usize {
        self.epochs.len() * size_of::<PendingEpoch>()
            + self
                .epochs
                .iter()
                .flat_map(PendingEpoch::runs)
                .map(PendingRun::charge)
                .sum::<usize>()
    }

    pub(super) fn epoch(&self, file: u64) -> Option<&PendingEpoch> {
        self.epochs.iter().find(|epoch| epoch.file == file)
    }

    /// Reverse the prepended runs once, so replay scans them in linear time.
    pub(super) fn prepare(&mut self, file: u64) {
        if let Some(epoch) = self.epochs.iter_mut().find(|epoch| epoch.file == file) {
            let mut ordered = None;
            while let Some(mut run) = epoch.runs.take() {
                epoch.runs = run.next.take();
                run.next = ordered;
                ordered = Some(run);
            }
            epoch.runs = ordered;
        }
    }

    pub(super) fn release(&mut self, file: u64) {
        if let Some(epoch) = self.epochs.iter_mut().find(|epoch| epoch.file == file) {
            let (total, payload) = epoch.clear();
            self.used -= total;
            self.payload -= payload;
            epoch.stopped = true;
        }
    }

    pub(super) fn read_object(
        &mut self,
        reader: &mut SequentialReader<'_>,
        map: &ScopedFilemarkMap,
        object: &TapeFileMapEntry,
    ) -> Result<(), ParityError> {
        let _phase = reader
            .source
            .verification_context()
            .phase(remanence_library::verification_io::VerificationPhase::ProtectedData);

        for epoch in self.epochs.iter_mut().filter(|epoch| !epoch.stopped) {
            let Some((first, lo, hi)) = overlap(object, epoch.start, epoch.end)? else {
                continue;
            };
            let charge = usize::try_from(hi - lo)
                .ok()
                .and_then(|n| n.checked_mul(9))
                .and_then(|n| n.checked_add(size_of::<PendingRun>()));
            let Some(charge) = charge.filter(|&n| n <= self.budget - self.used) else {
                // Retain earlier observations, including failures; never retry them.
                epoch.stopped = true;
                continue;
            };
            self.used += charge;
            let n = (hi - lo) as usize;
            self.payload += n * 9;
            self.high_water = self.high_water.max(self.used);
            self.payload_high_water = self.payload_high_water.max(self.payload);
            let mut pending = Box::new(PendingRun {
                file: object.tape_file_number,
                tags: vec![0; n].into_boxed_slice(),
                payloads: vec![0; n].into_boxed_slice(),
                next: None,
            });
            let mut run = crate::raw::ReadRun::new(reader.source, reader.block_size, hi - lo);
            run.continue_at(reader.next.take());
            let mut forward = SequentialReader::new(&mut run, reader.block_size);
            for ordinal in lo..hi {
                let physical = map.map.physical_position(TapeFilePosition {
                    tape_file_number: object.tape_file_number,
                    block_within_file: ordinal - first,
                })?;
                let observed = DataObservation::from_probe(forward.read(physical)?);
                let offset = (ordinal - lo) as usize;
                pending.tags[offset] = observed.tag;
                pending.payloads[offset] = observed.payload;
            }
            reader.next = forward.next;
            pending.next = epoch.runs.take();
            epoch.runs = Some(pending);
        }
        Ok(())
    }
}
