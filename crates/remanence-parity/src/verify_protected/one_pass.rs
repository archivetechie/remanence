//! Provisional protected checks consume the physical prefix reader's observations.
//! Only owned CRCs and bounded metadata windows survive a block callback.

use super::*;
use crate::scan::forward_file::FileObservation;
use crate::scan::{PrefixObserver, ScanBootstrapCandidate};
use crate::tape_index_replica::TapeIndexReplicaMapEntry;
use std::collections::BTreeMap;

/// All provisional state belongs to one route attempt and drops on disagreement.
pub(crate) struct PrefixChecks<'a> {
    pub(crate) map: ScopedFilemarkMap,
    pub(crate) rows: &'a [Vec<TapeIndexReplicaMapEntry>; 3],
    pub(crate) end: u64,
    pub(crate) uuid: [u8; 16],
    pub(crate) block_size: u32,
    pub(crate) scheme: Option<ParityScheme>,
    pub(crate) result: ProtectedContentVerification,
    pub(crate) bootstrap_findings: Vec<String>,
    pending: ForwardPass,
    directory: crate::recovery::DirectoryCache,
    shards: BTreeMap<u64, DataObservation>,
    file_start: u64,
}

impl<'a> PrefixChecks<'a> {
    pub(crate) fn new(
        map: ScopedFilemarkMap,
        rows: &'a [Vec<TapeIndexReplicaMapEntry>; 3],
        end: u64,
        uuid: [u8; 16],
        block_size: u32,
        budget: usize,
        directory: crate::recovery::DirectoryCache,
    ) -> Result<Self, ParityError> {
        let pending = ForwardPass::plan(&map, budget)?;
        Ok(Self {
            map,
            rows,
            end,
            uuid,
            block_size,
            pending,
            scheme: None,
            result: ProtectedContentVerification::none_protected(),
            bootstrap_findings: Vec::new(),
            directory,
            shards: BTreeMap::new(),
            file_start: 0,
        })
    }
}

impl PrefixObserver for PrefixChecks<'_> {
    fn bootstrap(&mut self, candidate: &ScanBootstrapCandidate) {
        self.bootstrap_findings
            .extend(bootstrap_fill_finding(candidate));
    }
    fn end_lba(&self) -> u64 {
        self.end
    }

    fn read_through(&self, file: u64) -> bool {
        let Some(entry) = self.map.map.entries().get(file as usize) else {
            return false;
        };
        if entry.kind != TapeFileKind::Object {
            return matches!(
                entry.kind,
                TapeFileKind::Bootstrap | TapeFileKind::ParitySidecar | TapeFileKind::ParityMap
            );
        }
        // Sidecar rows permit reads even when bootstrap disables parity;
        // every rejection still runs the walk.
        let Some(mut covered) = entry.first_parity_data_ordinal else {
            return false;
        };
        let Some(end) = covered.checked_add(entry.block_count) else {
            return false;
        };
        let mut ranges: Vec<_> = self
            .map
            .map
            .entries()
            .iter()
            .filter(|e| e.kind == TapeFileKind::ParitySidecar)
            .filter_map(|e| {
                Some((
                    e.protected_ordinal_start?,
                    e.protected_ordinal_end_exclusive?,
                ))
            })
            .collect();
        ranges.sort_unstable();
        for (lo, hi) in ranges {
            if lo <= covered {
                covered = covered.max(hi);
            }
        }
        covered >= end
    }

    fn start_file(&mut self, file: u64) -> Result<(usize, usize), ParityError> {
        self.shards.clear();
        let Some(entry) = self.map.map.entries().get(file as usize) else {
            return Ok((1, 1));
        };
        self.file_start = self
            .map
            .map
            .physical_position(TapeFilePosition {
                tape_file_number: file,
                block_within_file: 0,
            })?
            .lba;
        self.pending.begin_object(entry)?;
        // One file's windows share the sidecar retention ceiling.
        let limit = (PENDING_BUDGET / self.block_size as usize / 2).max(1);
        if entry.kind == TapeFileKind::ParityMap {
            return Ok((
                usize::try_from(entry.block_count)
                    .unwrap_or(usize::MAX)
                    .min(limit),
                1,
            ));
        }
        if entry.kind == TapeFileKind::ParitySidecar {
            if let Some((h, _)) = self.scheme.as_ref().and_then(|scheme| {
                sidecar_geometry_from_total(entry.block_count, scheme_parity_blocks(scheme))
            }) {
                let h = usize::try_from(h).unwrap_or(usize::MAX);
                return Ok((h.min(limit), h.saturating_add(1).min(limit)));
            }
        }
        Ok((1, 1))
    }

    fn block(
        &mut self,
        file: u64,
        position: PhysicalPositionHint,
        bytes: &[u8],
    ) -> Result<(), ParityError> {
        let Some(entry) = self.map.map.entries().get(file as usize) else {
            return Ok(());
        };
        let Some(offset) = position.lba.checked_sub(self.file_start) else {
            return Ok(());
        };
        if offset >= entry.block_count {
            return Ok(());
        }
        if entry.kind == TapeFileKind::Object {
            self.pending.observe(entry, offset, bytes);
        } else if entry.kind == TapeFileKind::ParitySidecar {
            if let Some((h, tail)) = self.scheme.as_ref().and_then(|scheme| {
                sidecar_geometry_from_total(entry.block_count, scheme_parity_blocks(scheme))
            }) {
                if (h..tail).contains(&offset) {
                    self.shards.insert(
                        offset,
                        DataObservation::from_parity_probe(Probe::Block(bytes)),
                    );
                }
            }
        }
        Ok(())
    }

    fn finish_file(
        &mut self,
        source: &mut dyn RawTapeSource,
        observation: FileObservation,
        measured: &[TapeFileMapEntry],
        candidates: &[ScanBootstrapCandidate],
    ) -> Result<bool, ParityError> {
        let file = observation.tape_file_number as usize;
        if observation.requires_spacing.is_some() {
            self.pending.discard_object(observation.tape_file_number);
            self.shards.clear();
        }
        for rows in self.rows {
            let row = rows.get(file).ok_or_else(|| {
                ParityError::FilemarkMapReconstruct("measured prefix has extra file".into())
            })?;
            crate::terminal_inventory::check_row_against_measured(row, file, measured)
                .map_err(ParityError::FilemarkMapReconstruct)?;
        }
        if observation.tape_file_number == 0 {
            self.scheme = crate::terminal_inventory::verification_scheme(candidates, None)
                .map_err(ParityError::FilemarkMapReconstruct)?;
        }
        let Some(scheme) = &self.scheme else {
            return Ok(false);
        };
        let entry = &self.map.map.entries()[file];
        if !self.map.is_validated(entry.tape_file_number) {
            return Ok(false);
        }
        let mut retained =
            retained::RetainedSource::from_file(source, self.block_size, &observation);
        match entry.kind {
            TapeFileKind::ParityMap => {
                let _phase = retained
                    .verification_context()
                    .phase(remanence_library::verification_io::VerificationPhase::ParityMap);
                let blocks = crate::parity_map::read_parity_map_blocks(
                    &mut retained,
                    &self.map.map,
                    entry,
                    self.block_size,
                )?;
                self.result.parity_map_findings.extend(
                    crate::parity_map::parity_map_damage(&blocks, &self.uuid)
                        .into_iter()
                        .map(|detail| ParityMapFinding {
                            tape_file_number: entry.tape_file_number,
                            detail,
                        }),
                );
            }
            TapeFileKind::ParitySidecar => {
                self.pending.prepare(entry.tape_file_number);
                self.result.sidecars.push(verify_sidecar_observed::<false>(
                    &mut retained,
                    &self.map,
                    entry,
                    scheme,
                    &self.uuid,
                    self.block_size,
                    &mut self.directory,
                    self.pending.epoch(entry.tape_file_number),
                    Some(std::mem::take(&mut self.shards)),
                )?);
                self.pending.release(entry.tape_file_number);
            }
            _ => {}
        }
        Ok(retained.positioned)
    }
}
