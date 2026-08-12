//! Receive-to-tape overlap sink and parity-object streaming entry point.

use std::sync::Arc;
use std::time::Instant;

use remanence_library::{
    BlockSink, TapeIoError, TapePosition, WriteFilemarksOutcome, WriteOutcome,
};

use super::model::PoolWriteError;

#[cfg(test)]
use super::capacity::{parity_capacity_basis_blocks, reserve_parity_object_capacity};
#[cfg(test)]
use super::model::{
    AppendCommitDiagnostics, PoolWriteResult, SelectedTape, WriteObjectToPoolRequest,
};
#[cfg(test)]
use super::no_parity::{commit_pool_write, pool_write_result, CommitPoolWriteProjection};
#[cfg(test)]
use super::prepare::{
    log_commit_diagnostics, log_transfer_diagnostics, open_prepared_readers,
    write_canonical_plaintext_object_to_parity, write_encrypted_object_to_parity,
    PreparedPoolWrite, PreparedStoredObject, TransferDiagnosticOutcome,
};
#[cfg(test)]
use super::staging::{run_counted_fenced_staged_transfer, CountingBlockSink};
#[cfg(test)]
use remanence_parity::{
    BlockSinkRawTapeSink, CommittedBundle, CommittedBundleKind, CommittedState, JournalError,
    ParityScheme, ParitySink, TapeFileJournal,
};
#[cfg(test)]
use remanence_state::{CatalogIndex, TapePoolConfig};
#[cfg(test)]
use remanence_stream::{write_prepared_object_to_parity_from_readers, StreamingObjectWriteReport};

/// Write-side hysteresis gate layered immediately above the existing bounded
/// hardware staging funnel. A pause flushes the current safe batch, waits for
/// the receive ring to refill, and re-proves the exact next physical LBA.
pub(crate) struct OverlapBlockSink<'a> {
    pub(crate) inner: &'a mut dyn BlockSink,
    pub(crate) control: Arc<crate::append_ring::AppendRingControl>,
    pub(crate) expected_initial_lba: u64,
    pub(crate) expected_next_lba: u64,
    pub(crate) initial_position_proved: bool,
    pub(crate) write_started: bool,
    pub(crate) low_water_events: u64,
}

impl OverlapBlockSink<'_> {
    pub(crate) fn ensure_prefill(&self) -> Result<(), TapeIoError> {
        if self.control.prefill_satisfied() {
            Ok(())
        } else {
            Err(TapeIoError::OperationFailed(
                "overlap first-block gate reached before high-water prefill and live-source validation"
                    .to_string(),
            ))
        }
    }

    pub(crate) fn prove_position(
        &self,
        observed: TapePosition,
        expected_lba: u64,
        context: &str,
    ) -> Result<(), TapeIoError> {
        if observed.partition != 0 || observed.lba != expected_lba {
            return Err(TapeIoError::OperationFailed(format!(
                "overlap write position drift during {context}: expected partition 0 lba {expected_lba}, observed partition {} lba {}",
                observed.partition, observed.lba
            )));
        }
        Ok(())
    }

    pub(crate) fn prove_initial_position(&mut self) -> Result<(), TapeIoError> {
        if !self.initial_position_proved {
            self.ensure_prefill()?;
            let observed = self.inner.position()?;
            self.prove_position(observed, self.expected_initial_lba, "first-block gate")?;
            self.initial_position_proved = true;
        }
        Ok(())
    }

    pub(crate) fn pause_if_low(&mut self) -> Result<(), TapeIoError> {
        if !self.write_started || !self.control.should_pause() {
            return Ok(());
        }
        let expected = self.expected_next_lba;
        let before_pause = self.inner.position()?;
        self.prove_position(before_pause, expected, "low-water pause boundary")?;
        self.low_water_events = self.low_water_events.saturating_add(1);
        let pause_started = Instant::now();
        tracing::info!(
            target: "remanence_write_diag",
            phase = "overlap_pause",
            low_water_events = self.low_water_events,
            ring_occupancy_bytes = self.control.occupancy_bytes(),
            ring_low_bytes = self.control.low_bytes(),
            expected_next_lba = expected,
            "remanence_write_diag",
        );
        self.control
            .wait_for_resume()
            .map_err(|err| TapeIoError::OperationFailed(format!("overlap refill failed: {err}")))?;
        let observed = self.inner.position()?;
        let proof = self.prove_position(observed, expected, "low-water resume");
        tracing::info!(
            target: "remanence_write_diag",
            phase = "overlap_resume_proof",
            low_water_events = self.low_water_events,
            ring_occupancy_bytes = self.control.occupancy_bytes(),
            ring_high_bytes = self.control.high_bytes(),
            pause_duration_ms = crate::diagnostics::duration_ms(pause_started.elapsed()),
            expected_next_lba = expected,
            observed_next_lba = observed.lba,
            resume_proof_ok = proof.is_ok(),
            "remanence_write_diag",
        );
        proof
    }
}

impl BlockSink for OverlapBlockSink<'_> {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        self.prove_initial_position()?;
        if let Some(message) = self.control.failure_message() {
            return Err(TapeIoError::OperationFailed(format!(
                "overlap source failed before WRITE submission: {message}"
            )));
        }
        self.pause_if_low()?;
        self.write_started = true;
        let outcome = self.inner.write_block(buf)?;
        let expected = self.expected_next_lba.checked_add(1).ok_or_else(|| {
            TapeIoError::OperationFailed("overlap expected next LBA overflow".to_string())
        })?;
        self.prove_position(outcome.position_after, expected, "WRITE completion")?;
        self.expected_next_lba = expected;
        Ok(outcome)
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.inner.write_filemarks(count)
    }

    fn write_filemarks_immediate(&mut self, count: u32) -> Result<(), TapeIoError> {
        self.inner.write_filemarks_immediate(count)
    }

    fn space_to_end_of_data(&mut self) -> Result<TapePosition, TapeIoError> {
        self.ensure_prefill()?;
        let observed = self.inner.space_to_end_of_data()?;
        self.prove_position(observed, self.expected_initial_lba, "append-position gate")?;
        self.initial_position_proved = true;
        Ok(observed)
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        self.ensure_prefill()?;
        let observed = self.inner.locate(lba)?;
        self.prove_position(observed, lba, "checkpoint recovery LOCATE")?;
        self.expected_next_lba = lba;
        self.initial_position_proved = true;
        Ok(observed)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.position()
    }
}

pub(crate) fn with_overlap_sink<R>(
    inner: &mut dyn BlockSink,
    control: Option<Arc<crate::append_ring::AppendRingControl>>,
    expected_initial_lba: u64,
    operation: impl FnOnce(&mut dyn BlockSink) -> Result<R, PoolWriteError>,
) -> Result<R, PoolWriteError> {
    match control {
        Some(control) => {
            let mut gated = OverlapBlockSink {
                inner,
                control,
                expected_initial_lba,
                expected_next_lba: expected_initial_lba,
                initial_position_proved: false,
                write_started: false,
                low_water_events: 0,
            };
            operation(&mut gated)
        }
        None => operation(inner),
    }
}

#[cfg(test)]
pub(crate) struct PerObjectTestJournal {
    pub(crate) tape_uuid: [u8; 16],
    pub(crate) bundles: Vec<CommittedBundle>,
}

#[cfg(test)]
impl TapeFileJournal for PerObjectTestJournal {
    fn tape_uuid(&self) -> [u8; 16] {
        self.tape_uuid
    }

    fn commit_bundle(&mut self, bundle: &CommittedBundle) -> Result<(), JournalError> {
        self.bundles.push(bundle.clone());
        Ok(())
    }

    fn load_committed(&self) -> Result<CommittedState, JournalError> {
        let retained_end = self
            .bundles
            .iter()
            .rposition(|bundle| bundle.kind == CommittedBundleKind::CheckpointedThrough)
            .map_or(0, |index| index + 1);
        let retained = &self.bundles[..retained_end];
        let last = retained
            .iter()
            .rev()
            .find(|bundle| bundle.kind != CommittedBundleKind::CheckpointedThrough);
        Ok(CommittedState {
            entries: retained
                .iter()
                .filter(|bundle| bundle.kind != CommittedBundleKind::CheckpointedThrough)
                .flat_map(|bundle| bundle.entries.iter().cloned())
                .collect(),
            highest_protected_ordinal: last.map_or(0, |bundle| bundle.highest_protected_ordinal),
            total_committed_ordinals: last.map_or(0, |bundle| bundle.total_committed_ordinals),
            orphaned_bundles: self.bundles[retained_end..].to_vec(),
        })
    }
}

#[cfg(test)]
pub(crate) fn write_parity_object_to_selected_tape<S: BlockSink + ?Sized>(
    state: &mut CatalogIndex,
    sink: &mut CountingBlockSink<'_, S>,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    prepared_write: PreparedPoolWrite,
    scheme: ParityScheme,
) -> Result<PoolWriteResult, PoolWriteError> {
    let PreparedPoolWrite { prepared, stored } = prepared_write;
    let tape_uuid = selected.tape_uuid;
    let block_size = selected.block_size;
    let mut parity_journal = PerObjectTestJournal {
        tape_uuid,
        bundles: Vec::new(),
    };
    let overlap_control = prepared.overlap_control();
    let capacity_blocks = parity_capacity_basis_blocks(state, pool_cfg, &selected)?;
    let io_memory = crate::io_memory::IoMemoryReservation::new(
        remanence_state::DEFAULT_IO_MEMORY_CEILING_BYTES,
    )
    .map_err(PoolWriteError::InvalidInput)?;
    let transfer_started = Instant::now();
    let write_report: Result<StreamingObjectWriteReport, PoolWriteError> =
        run_counted_fenced_staged_transfer(
            state,
            &selected,
            sink,
            block_size as usize,
            overlap_control.as_ref().map(Arc::clone),
            |staged| {
                with_overlap_sink(staged, overlap_control, 0, |gated| {
                    let mut raw = BlockSinkRawTapeSink::new(gated);
                    let mut parity = ParitySink::new_with_journal(
                        &mut raw,
                        &mut parity_journal,
                        scheme.clone(),
                        tape_uuid,
                        block_size,
                    )?;
                    parity.write_bootstrap()?;
                    let report = match &stored {
                        PreparedStoredObject::Plaintext => {
                            let mut readers = open_prepared_readers(&prepared)?;
                            let capacity = reserve_parity_object_capacity(
                                parity.terminal_triple_capacity_runtime_state()?,
                                parity.scheme(),
                                &selected,
                                (pool_cfg, 1, 0),
                                capacity_blocks,
                                prepared.plan.layout.projected_size_blocks,
                                &io_memory,
                            )?;
                            let (capacity, _spool_permit) = capacity.into_parts();
                            Ok(write_prepared_object_to_parity_from_readers(
                                &mut parity,
                                tape_uuid,
                                &prepared.options,
                                &prepared.files,
                                &mut readers,
                                capacity,
                            )?)
                        }
                        PreparedStoredObject::CanonicalPlaintext => {
                            let capacity = reserve_parity_object_capacity(
                                parity.terminal_triple_capacity_runtime_state()?,
                                parity.scheme(),
                                &selected,
                                (pool_cfg, 1, 0),
                                capacity_blocks,
                                prepared.plan.layout.projected_size_blocks,
                                &io_memory,
                            )?;
                            let (capacity, _spool_permit) = capacity.into_parts();
                            write_canonical_plaintext_object_to_parity(
                                &mut parity,
                                tape_uuid,
                                &prepared,
                                capacity,
                            )
                        }
                        PreparedStoredObject::Encrypted(encrypted) => {
                            let capacity = reserve_parity_object_capacity(
                                parity.terminal_triple_capacity_runtime_state()?,
                                parity.scheme(),
                                &selected,
                                (pool_cfg, 1, 0),
                                capacity_blocks,
                                encrypted.envelope.stored_size_blocks,
                                &io_memory,
                            )?;
                            let (capacity, _spool_permit) = capacity.into_parts();
                            write_encrypted_object_to_parity(
                                &mut parity,
                                tape_uuid,
                                &prepared,
                                encrypted,
                                capacity,
                            )
                        }
                    }?;
                    Ok(report)
                })
            },
        );
    let transfer_elapsed = transfer_started.elapsed();
    let write_report = match write_report {
        Ok(write_report) => {
            let stats = sink.stats();
            log_transfer_diagnostics(
                &request,
                &selected,
                &prepared,
                stored.projected_size_blocks(&prepared),
                false,
                TransferDiagnosticOutcome {
                    stats,
                    elapsed: transfer_elapsed,
                    status: "ok",
                    error: None,
                },
            );
            (write_report, stats)
        }
        Err(err) => {
            let error = err.to_string();
            log_transfer_diagnostics(
                &request,
                &selected,
                &prepared,
                stored.projected_size_blocks(&prepared),
                false,
                TransferDiagnosticOutcome {
                    stats: sink.stats(),
                    elapsed: transfer_elapsed,
                    status: "error",
                    error: Some(error.as_str()),
                },
            );
            return Err(err);
        }
    };
    let (write_report, transfer_stats) = write_report;

    let commit_started = Instant::now();
    let commit_result = commit_pool_write(
        state,
        &selected,
        &prepared,
        &write_report,
        CommitPoolWriteProjection {
            first_parity_data_ordinal: write_report.catalog.object_copy.first_parity_data_ordinal,
            protected_until_ordinal: write_report.catalog.object_copy.protected_until_ordinal,
            scheme: Some(scheme),
            copy_representation: stored.copy_representation(),
        },
        pool_cfg,
        transfer_stats.early_warning,
    );
    let commit_elapsed = commit_started.elapsed();
    let sealed_after_write = match commit_result {
        Ok(sealed_after_write) => {
            log_commit_diagnostics(&request, &selected, &prepared, commit_elapsed, "ok", None);
            sealed_after_write
        }
        Err(err) => {
            let error = err.to_string();
            log_commit_diagnostics(
                &request,
                &selected,
                &prepared,
                commit_elapsed,
                "error",
                Some(error.as_str()),
            );
            return Err(err);
        }
    };
    pool_write_result(
        request,
        selected,
        prepared,
        stored.copy_representation(),
        write_report,
        AppendCommitDiagnostics {
            filemark_write_drain: transfer_stats.filemark_write_drain,
            catalog_journal_fsync: commit_elapsed,
        },
        sealed_after_write,
        None,
    )
}
