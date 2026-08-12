//! Behavior-preserving operational cluster extracted from the parent module.

use std::io::Write;
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::time::{Duration as StdDuration, Instant};

use remanence_format::error::FormatError;
use remanence_format::model::BodyLba;
use remanence_library::{
    BlockSource, DriveHandle, DriveHandleSource, ReadBatchOutcome, SpaceKind, SpaceResult,
    TapeIoError, TapePosition,
};
use remanence_parity::ParityError;
use remanence_state::{CatalogIndex, NativeObjectFileRecord, TapeIoConfig};
use remanence_stream::StreamingError;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tonic::Status;
use uuid::Uuid;

use super::actor_protocol::ReadResumeTarget;
use super::actor_runtime::WriteOwnerConfig;
use super::checkpoint::PendingCheckpointBatch;
use crate::{
    pb, status_from_state_error, timestamp_from_rfc3339, verify_tape_identity, PoolWriteError,
    SelectTapeError, TapeUuid,
};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RestoreReadPhases {
    pub(crate) position: StdDuration,
    pub(crate) transfer: StdDuration,
    pub(crate) bytes: u64,
    pub(crate) records: u64,
    pub(crate) commands: u64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RestoreDiagnosticContext {
    pub(crate) session_id: Uuid,
    pub(crate) tape_uuid: [u8; 16],
    pub(crate) block_size_bytes: u32,
    pub(crate) success: bool,
}

/// Times the existing `BlockSource` safety funnel without reimplementing any
/// tape operation. Every call delegates exactly once to the wrapped source.
pub(crate) struct DiagnosticBlockSource<'a> {
    pub(crate) inner: &'a mut dyn BlockSource,
    pub(crate) phases: RestoreReadPhases,
}

impl<'a> DiagnosticBlockSource<'a> {
    pub(crate) fn new(inner: &'a mut dyn BlockSource) -> Self {
        Self {
            inner,
            phases: RestoreReadPhases::default(),
        }
    }

    pub(crate) fn phases(&self) -> RestoreReadPhases {
        self.phases
    }
}

impl remanence_library::BlockRead for DiagnosticBlockSource<'_> {
    fn read_block(&mut self, buf: &mut [u8]) -> Result<usize, TapeIoError> {
        let started = Instant::now();
        let result = self.inner.read_block(buf);
        self.phases.transfer += started.elapsed();
        if let Ok(bytes) = result {
            self.phases.commands = self.phases.commands.saturating_add(1);
            self.phases.records = self.phases.records.saturating_add(1);
            self.phases.bytes = self.phases.bytes.saturating_add(bytes as u64);
        }
        result
    }
}

impl BlockSource for DiagnosticBlockSource<'_> {
    fn read_block_batch(
        &mut self,
        buf: &mut [u8],
        block_size_bytes: u32,
        requested_records: u32,
        remaining_records_in_file: u32,
    ) -> Result<ReadBatchOutcome, TapeIoError> {
        let started = Instant::now();
        let result = self.inner.read_block_batch(
            buf,
            block_size_bytes,
            requested_records,
            remaining_records_in_file,
        );
        self.phases.transfer += started.elapsed();
        if let Ok(outcome) = result {
            self.phases.commands = self.phases.commands.saturating_add(1);
            self.phases.records = self
                .phases
                .records
                .saturating_add(u64::from(outcome.records_read));
            self.phases.bytes = self
                .phases
                .bytes
                .saturating_add(u64::from(outcome.bytes_read));
        }
        result
    }

    fn read_batch_blocks(&self, block_size_bytes: u32) -> u32 {
        self.inner.read_batch_blocks(block_size_bytes)
    }

    fn read_ring_buffers(&self) -> u32 {
        self.inner.read_ring_buffers()
    }

    fn prove_read_position(
        &mut self,
        expected: TapePosition,
    ) -> Result<remanence_library::DevicePositionProof, TapeIoError> {
        let started = Instant::now();
        let result = self.inner.prove_read_position(expected);
        self.phases.position += started.elapsed();
        result
    }

    fn rewind(&mut self) -> Result<(), TapeIoError> {
        let started = Instant::now();
        let result = self.inner.rewind();
        self.phases.position += started.elapsed();
        result
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        let started = Instant::now();
        let result = self.inner.locate(lba);
        self.phases.position += started.elapsed();
        result
    }

    fn space(&mut self, count: i64, kind: SpaceKind) -> Result<SpaceResult, TapeIoError> {
        let started = Instant::now();
        let result = self.inner.space(count, kind);
        self.phases.position += started.elapsed();
        result
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        let started = Instant::now();
        let result = self.inner.position();
        self.phases.position += started.elapsed();
        result
    }
}

pub(crate) fn log_restore_read_diagnostics(
    drive: &DriveHandle,
    context: RestoreDiagnosticContext,
    phases: RestoreReadPhases,
    relay_diagnostics: StagedReadRelayDiagnostics,
    wall: StdDuration,
) {
    let relay = relay_diagnostics.client_write;
    let phase_sum = wall;
    let bottleneck = if phases.transfer >= relay {
        "drive"
    } else {
        "sender"
    };
    let diagnostics = drive.pipelined_read_diagnostics();
    let effective_batch_blocks = drive.requested_read_batch_blocks().min(
        drive
            .sg_reserved_size_bytes()
            .checked_div(context.block_size_bytes.max(1))
            .unwrap_or(1)
            .max(1),
    );
    let batch_effectiveness = if phases.commands == 0 {
        0.0
    } else {
        phases.records as f64 / phases.commands as f64
    };
    tracing::info!(
        target: "remanence_read_diag",
        phase = "restore_total",
        session_id = %context.session_id,
        tape_uuid = %Uuid::from_bytes(context.tape_uuid),
        status = if context.success { "ok" } else { "error" },
        effective_mode = "fixed_pipelined",
        block_size_bytes = context.block_size_bytes,
        staging_ring_buffers = drive.staging_ring_buffers(),
        effective_batch_blocks,
        batch_effectiveness_records_per_command = batch_effectiveness,
        bytes = phases.bytes,
        records = phases.records,
        commands = phases.commands,
        locate_position_ms = crate::diagnostics::duration_ms(phases.position),
        transfer_ms = crate::diagnostics::duration_ms(phases.transfer),
        relay_ms = crate::diagnostics::duration_ms(relay),
        phase_sum_ms = crate::diagnostics::duration_ms(phase_sum),
        wall_ms = crate::diagnostics::duration_ms(wall),
        bottleneck,
        drive_rate_mib_s = crate::diagnostics::mib_per_s(phases.bytes, phases.transfer),
        relay_rate_mib_s = crate::diagnostics::mib_per_s(phases.bytes, relay),
        client_write_ms = crate::diagnostics::duration_ms(relay_diagnostics.client_write),
        sender_stall_ms = crate::diagnostics::duration_ms(relay_diagnostics.sender_stall),
        client_write_bytes = relay_diagnostics.bytes,
        client_write_rate_mib_s = crate::diagnostics::mib_per_s(
            relay_diagnostics.bytes,
            relay_diagnostics.client_write,
        ),
        gap_samples = diagnostics.gap_samples,
        ioctl_samples = diagnostics.ioctl_samples,
        gap_p50_us = diagnostics.gap_p50_us,
        gap_p95_us = diagnostics.gap_p95_us,
        gap_max_us = diagnostics.gap_max_us,
        ioctl_p50_us = diagnostics.ioctl_p50_us,
        ioctl_p95_us = diagnostics.ioctl_p95_us,
        ioctl_max_us = diagnostics.ioctl_max_us,
        ioctl_mean_us = diagnostics.ioctl_mean_us,
        first_60s_ioctl_samples = diagnostics.first_60s_ioctl_samples,
        first_60s_ioctl_p50_us = diagnostics.first_60s_ioctl_p50_us,
        first_60s_ioctl_p95_us = diagnostics.first_60s_ioctl_p95_us,
        first_60s_ioctl_max_us = diagnostics.first_60s_ioctl_max_us,
        first_60s_ioctl_mean_us = diagnostics.first_60s_ioctl_mean_us,
        accounting_samples = diagnostics.accounting_samples,
        accounting_p50_us = diagnostics.accounting_p50_us,
        accounting_p95_us = diagnostics.accounting_p95_us,
        accounting_max_us = diagnostics.accounting_max_us,
        accounting_mean_us = diagnostics.accounting_mean_us,
        cadence_us = diagnostics.cadence_us,
        effective_feed_bytes_per_second = diagnostics.effective_feed_bytes_per_second,
        time_to_first_ioctl_ms = diagnostics.time_to_first_ioctl_ms,
        steady_reached = diagnostics.steady_reached,
        time_to_steady_ms = diagnostics.time_to_steady_ms,
        steady_window_seconds = diagnostics.steady_window_seconds,
        steady_threshold_percent = diagnostics.steady_threshold_percent,
        ramp_observation_seconds = diagnostics.ramp_observation_seconds,
        "remanence_read_diag",
    );
}

#[cfg(test)]
pub(crate) fn exclusive_restore_relay_phase(
    wall: StdDuration,
    position: StdDuration,
    transfer: StdDuration,
) -> StdDuration {
    wall.saturating_sub(position).saturating_sub(transfer)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn stream_one_object(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    cfg: &WriteOwnerConfig,
    session_id: Uuid,
    tape_uuid: &[u8; 16],
    object_id: &str,
    stream_chunk_bytes: u32,
    chunk_tx: crate::read_core::ReadStreamSender,
) -> Result<(), Status> {
    let object = index
        .get_native_object(object_id)
        .map_err(status_from_state_error)?;
    let object = object.ok_or_else(|| Status::not_found("object not found"))?;
    let manifest_sha256 = object
        .metadata_hash
        .as_deref()
        .map(|hash| {
            <[u8; 32]>::try_from(hash)
                .map_err(|_| Status::internal("catalog metadata_hash is not 32 bytes"))
        })
        .transpose()?;
    let copy = object
        .copies
        .iter()
        .find(|copy| copy.tape_uuid.as_slice() == tape_uuid)
        .ok_or_else(|| {
            Status::failed_precondition("object is not on the tape pinned by this read session")
        })?;
    let tape_files = index
        .list_tape_files(tape_uuid)
        .map_err(status_from_state_error)?;
    let tape_file = tape_files
        .iter()
        .find(|file| {
            file.tape_file_number == copy.tape_file_number
                && file.kind == "object"
                && file.object_id.as_deref() == Some(object_id)
        })
        .ok_or_else(|| Status::not_found("object tape file not in catalog"))?;
    let tape = index
        .get_tape(tape_uuid)
        .map_err(status_from_state_error)?
        .ok_or_else(|| Status::not_found("tape not found"))?;
    let block_size = tape
        .block_size
        .ok_or_else(|| Status::internal("tape block size unknown"))?;
    let block_size_usize = usize::try_from(block_size)
        .map_err(|_| Status::internal("tape block size does not fit usize"))?;

    let block_size_u32 = u32::try_from(block_size)
        .map_err(|_| Status::internal("tape block size does not fit u32"))?;
    drive
        .rewind()
        .map_err(|err| Status::internal(format!("rewind before object read: {err}")))?;

    drive.reset_pipelined_diagnostics();
    let wall_started = Instant::now();
    let (result, phases) = {
        let mut source = DriveHandleSource(drive);
        let mut diagnostic_source = DiagnosticBlockSource::new(&mut source);
        let result = stream_with_staged_read_sender_diagnostics(
            chunk_tx,
            stream_chunk_bytes,
            |writer, terminal| {
                let mut sink = crate::read_core::CapturePayloadSink::new(writer);
                crate::read_core::read_object_payload_with_pipeline(
                    &mut diagnostic_source,
                    block_size_usize,
                    tape_file.block_count,
                    copy.tape_file_number,
                    manifest_sha256,
                    &mut sink,
                    crate::read_core::ReadPipelineConfig {
                        reservoir_bytes: cfg.tape_io.read_reservoir_bytes,
                        high_pct: cfg.tape_io.read_reservoir_high_pct,
                        low_pct: cfg.tape_io.read_reservoir_low_pct,
                        ranged_frontier: false,
                        proof_cadence_bytes: cfg
                            .tape_io
                            .position_check_bytes_ranged
                            .min(cfg.tape_io.read_reservoir_bytes / 2),
                        terminal: Some(terminal),
                    },
                    Arc::clone(&cfg.io_memory),
                )
                .map_err(|err| Status::internal(format!("read object: {err}")))?;
                let (_payload_bytes, _digest) = sink
                    .finish()
                    .map_err(|err| Status::internal(format!("finish payload stream: {err}")))?;
                Ok(())
            },
        );
        (result, diagnostic_source.phases())
    };
    let wall = wall_started.elapsed();
    log_restore_read_diagnostics(
        drive,
        RestoreDiagnosticContext {
            session_id,
            tape_uuid: *tape_uuid,
            block_size_bytes: block_size_u32,
            success: result.is_ok(),
        },
        phases,
        result.as_ref().copied().unwrap_or_default(),
        wall,
    );
    result.map(|_| ())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn stream_one_file_range(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    cfg: &WriteOwnerConfig,
    session_id: Uuid,
    tape_uuid: &[u8; 16],
    object_id: &str,
    file_id: &str,
    start_byte: u64,
    end_byte: u64,
    stream_chunk_bytes: u32,
    chunk_tx: crate::read_core::ReadStreamSender,
) -> Result<(), Status> {
    let request =
        file_range_read_request(index, tape_uuid, object_id, file_id, start_byte, end_byte)?;
    let block_size_u32 = u32::try_from(request.block_size)
        .map_err(|_| Status::internal("tape block size does not fit u32"))?;

    drive.reset_pipelined_diagnostics();
    let wall_started = Instant::now();
    let (result, phases) = {
        let mut source = DriveHandleSource(drive);
        let mut diagnostic_source = DiagnosticBlockSource::new(&mut source);
        let result = stream_file_range_from_source(
            &mut diagnostic_source,
            request,
            stream_chunk_bytes,
            chunk_tx,
            &cfg.tape_io,
            Arc::clone(&cfg.io_memory),
        );
        (result, diagnostic_source.phases())
    };
    let wall = wall_started.elapsed();
    log_restore_read_diagnostics(
        drive,
        RestoreDiagnosticContext {
            session_id,
            tape_uuid: *tape_uuid,
            block_size_bytes: block_size_u32,
            success: result.is_ok(),
        },
        phases,
        result.as_ref().copied().unwrap_or_default(),
        wall,
    );
    result.map(|_| ())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn file_range_read_request(
    index: &CatalogIndex,
    tape_uuid: &[u8; 16],
    object_id: &str,
    file_id: &str,
    start_byte: u64,
    end_byte: u64,
) -> Result<crate::read_core::PlaintextFileRangeReadRequest, Status> {
    let object = index
        .get_native_object(object_id)
        .map_err(status_from_state_error)?;
    let object = object.ok_or_else(|| Status::not_found("object not found"))?;
    let file = resolve_object_file_for_range(index, object_id, file_id)?;
    let copy = object
        .copies
        .iter()
        .find(|copy| copy.tape_uuid.as_slice() == tape_uuid)
        .ok_or_else(|| {
            Status::failed_precondition("object is not on the tape pinned by this read session")
        })?;
    let (range_start, range_len) = requested_file_range(file.size_bytes, start_byte, end_byte)?;

    let tape_files = index
        .list_tape_files(tape_uuid)
        .map_err(status_from_state_error)?;
    let tape_file = tape_files
        .iter()
        .find(|tape_file| {
            tape_file.tape_file_number == copy.tape_file_number
                && tape_file.kind == "object"
                && tape_file.object_id.as_deref() == Some(object_id)
        })
        .ok_or_else(|| Status::not_found("object tape file not in catalog"))?;
    let tape = index
        .get_tape(tape_uuid)
        .map_err(status_from_state_error)?
        .ok_or_else(|| Status::not_found("tape not found"))?;
    let block_size = tape
        .block_size
        .ok_or_else(|| Status::internal("tape block size unknown"))?;
    let block_size_usize = usize::try_from(block_size)
        .map_err(|_| Status::internal("tape block size does not fit usize"))?;
    let physical_file_start_lba =
        derive_physical_file_start_lba(tape_files.as_slice(), tape_file.tape_file_number);
    Ok(crate::read_core::PlaintextFileRangeReadRequest {
        block_size: block_size_usize,
        tape_file_number: tape_file.tape_file_number,
        physical_file_start_lba,
        first_chunk_lba: file.first_chunk_lba.map(BodyLba),
        file_size_bytes: file.size_bytes,
        range_start,
        range_len,
    })
}

/// Derive an absolute tape-file start from the dense committed catalog prefix.
/// Each trailing filemark consumes one physical LBA, matching the filemark-map
/// physical-position calculation. An incomplete or non-dense prefix returns
/// `None` so the range reader uses its logical REWIND/SPACE fallback.
pub(crate) fn derive_physical_file_start_lba(
    tape_files: &[remanence_state::TapeFileRecord],
    target_file_number: u64,
) -> Option<u64> {
    let mut expected_file_number = 0u64;
    let mut next_file_lba = 0u64;
    for tape_file in tape_files {
        if tape_file.tape_file_number != expected_file_number {
            return None;
        }
        if tape_file.tape_file_number == target_file_number {
            return Some(next_file_lba);
        }
        next_file_lba = next_file_lba
            .checked_add(tape_file.block_count)?
            .checked_add(1)?;
        expected_file_number = expected_file_number.checked_add(1)?;
    }
    None
}

pub(crate) fn resolve_object_file_for_range(
    index: &CatalogIndex,
    object_id: &str,
    file_id: &str,
) -> Result<NativeObjectFileRecord, Status> {
    if file_id.is_empty() {
        let files = index
            .list_native_object_files(object_id)
            .map_err(status_from_state_error)?;
        return match files.as_slice() {
            [file] => Ok(file.clone()),
            [] => Err(Status::failed_precondition(
                "empty file_id ranged reads require exactly one object file row; found 0",
            )),
            _ => Err(Status::failed_precondition(format!(
                "empty file_id ranged reads require exactly one object file row; found {}",
                files.len()
            ))),
        };
    }

    let file = index
        .get_native_object_file(object_id, file_id)
        .map_err(status_from_state_error)?;
    file.ok_or_else(|| Status::not_found("object file not found"))
}

pub(crate) fn stream_file_range_from_source(
    source: &mut dyn BlockSource,
    request: crate::read_core::PlaintextFileRangeReadRequest,
    stream_chunk_bytes: u32,
    chunk_tx: crate::read_core::ReadStreamSender,
    tape_io: &TapeIoConfig,
    io_memory: Arc<crate::io_memory::IoMemoryReservation>,
) -> Result<StagedReadRelayDiagnostics, Status> {
    // Ranged reads are opaque stored-payload reads. The daemon does not decrypt
    // or hold key material; clients interpret or decrypt the returned bytes.
    stream_with_staged_read_sender_diagnostics(chunk_tx, stream_chunk_bytes, |writer, terminal| {
        crate::read_core::read_plaintext_file_range_with_pipeline(
            source,
            request,
            writer,
            crate::read_core::ReadPipelineConfig {
                reservoir_bytes: tape_io.read_reservoir_bytes,
                high_pct: tape_io.read_reservoir_high_pct,
                low_pct: tape_io.read_reservoir_low_pct,
                ranged_frontier: true,
                proof_cadence_bytes: tape_io
                    .position_check_bytes_ranged
                    .min(tape_io.read_reservoir_bytes / 2),
                terminal: Some(terminal),
            },
            io_memory,
        )
        .map_err(status_from_file_range_error)
    })
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct StagedReadRelayDiagnostics {
    pub(crate) client_write: StdDuration,
    pub(crate) sender_stall: StdDuration,
    pub(crate) bytes: u64,
}

pub(crate) fn stream_with_staged_read_sender_diagnostics(
    chunk_tx: crate::read_core::ReadStreamSender,
    stream_chunk_bytes: u32,
    produce: impl FnOnce(
        &mut (dyn std::io::Write + Send),
        Arc<crate::read_core::ReadTerminalAccumulator>,
    ) -> Result<(), Status>,
) -> Result<StagedReadRelayDiagnostics, Status> {
    let staged_capacity = crate::read_core::read_stream_channel_capacity(
        usize::try_from(stream_chunk_bytes).unwrap_or(usize::MAX),
    );
    let (tx, rx) = std_mpsc::sync_channel(staged_capacity);
    let poison = Arc::new(Mutex::new(None::<String>));
    let terminal = Arc::new(crate::read_core::ReadTerminalAccumulator::default());
    std::thread::scope(|scope| {
        let sender_poison = Arc::clone(&poison);
        let sender_terminal = Arc::clone(&terminal);
        let sender = scope.spawn(move || {
            let result = drain_staged_read_sender(rx, chunk_tx, stream_chunk_bytes, sender_poison);
            if let Err(status) = &result {
                sender_terminal.record(
                    crate::read_core::ReadTerminalPriority::Sender,
                    status.clone(),
                );
            }
            result
        });
        let mut writer = StagedReadWriter::new(
            tx,
            Arc::clone(&poison),
            usize::try_from(stream_chunk_bytes).unwrap_or(usize::MAX),
        );
        let produce_result = produce(&mut writer, Arc::clone(&terminal)).and_then(|()| {
            writer
                .finish()
                .map_err(|err| Status::internal(format!("finish read stream: {err}")))
        });
        if let Err(status) = &produce_result {
            terminal.record(
                crate::read_core::ReadTerminalPriority::Decode,
                status.clone(),
            );
        }
        drop(writer);
        let sender_result = sender.join().unwrap_or_else(|_| {
            let status = Status::internal("staged read sender thread panicked");
            terminal.record(
                crate::read_core::ReadTerminalPriority::Sender,
                status.clone(),
            );
            Err(status)
        });
        match (produce_result, sender_result) {
            (Ok(()), Ok(diagnostics)) => Ok(diagnostics),
            _ => Err(terminal.finalize_after_join().unwrap_or_else(|| {
                Status::internal("read pipeline failed without terminal cause")
            })),
        }
    })
}

pub(crate) enum StagedReadItem {
    Data(Vec<u8>),
    Finish,
}

pub(crate) struct StagedReadWriter {
    pub(crate) tx: std_mpsc::SyncSender<StagedReadItem>,
    pub(crate) poison: Arc<Mutex<Option<String>>>,
    pub(crate) finished: bool,
    pub(crate) max_chunk_bytes: usize,
}

impl StagedReadWriter {
    pub(crate) fn new(
        tx: std_mpsc::SyncSender<StagedReadItem>,
        poison: Arc<Mutex<Option<String>>>,
        chunk_bytes: usize,
    ) -> Self {
        Self {
            tx,
            poison,
            finished: false,
            max_chunk_bytes: crate::read_core::effective_read_stream_chunk_bytes(chunk_bytes),
        }
    }

    pub(crate) fn check_poison(&self) -> std::io::Result<()> {
        if let Some(message) = self
            .poison
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
        {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                format!("staged read sender failed: {message}"),
            ))
        } else {
            Ok(())
        }
    }

    pub(crate) fn finish(&mut self) -> std::io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.check_poison()?;
        self.tx.send(StagedReadItem::Finish).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "staged read sender stopped")
        })?;
        self.finished = true;
        self.check_poison()
    }
}

impl std::io::Write for StagedReadWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.finished {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "staged read stream already finished",
            ));
        }
        self.check_poison()?;
        for chunk in buf.chunks(self.max_chunk_bytes) {
            self.tx
                .send(StagedReadItem::Data(chunk.to_vec()))
                .map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "staged read sender stopped",
                    )
                })?;
        }
        self.check_poison()?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.check_poison()
    }
}

pub(crate) fn drain_staged_read_sender(
    rx: std_mpsc::Receiver<StagedReadItem>,
    chunk_tx: crate::read_core::ReadStreamSender,
    stream_chunk_bytes: u32,
    poison: Arc<Mutex<Option<String>>>,
) -> Result<StagedReadRelayDiagnostics, Status> {
    let mut writer = Some(if stream_chunk_bytes == 0 {
        crate::read_core::ChannelWriter::new(chunk_tx)
    } else {
        crate::read_core::ChannelWriter::with_chunk_size(chunk_tx, stream_chunk_bytes as usize)
    });
    let mut first_error = None;
    let mut diagnostics = StagedReadRelayDiagnostics::default();
    while let Ok(item) = rx.recv() {
        if first_error.is_some() {
            continue;
        }
        let client_started = Instant::now();
        let result = match item {
            StagedReadItem::Data(bytes) => match writer.as_mut() {
                Some(writer) => {
                    let bytes_len = bytes.len() as u64;
                    let result = writer
                        .write_all(&bytes)
                        .map_err(|err| Status::internal(format!("send read stream: {err}")));
                    diagnostics.sender_stall = writer.sender_stall();
                    if result.is_ok() {
                        diagnostics.bytes = diagnostics.bytes.saturating_add(bytes_len);
                    }
                    result
                }
                None => Err(Status::internal("staged read data after finish")),
            },
            StagedReadItem::Finish => match writer.take() {
                Some(mut writer) => {
                    let result = writer
                        .finish()
                        .map_err(|err| Status::internal(format!("finish read stream: {err}")));
                    diagnostics.sender_stall = writer.sender_stall();
                    result
                }
                None => Ok(()),
            },
        };
        diagnostics.client_write += client_started.elapsed();
        if let Err(status) = result {
            set_staged_read_poison(&poison, status.message());
            first_error = Some(status);
        }
    }
    match first_error {
        Some(status) => Err(status),
        None => Ok(diagnostics),
    }
}

pub(crate) fn set_staged_read_poison(poison: &Arc<Mutex<Option<String>>>, message: &str) {
    let mut guard = poison.lock().unwrap_or_else(|err| err.into_inner());
    guard.get_or_insert_with(|| message.to_string());
}

pub(crate) fn requested_file_range(
    file_size_bytes: u64,
    start_byte: u64,
    end_byte: u64,
) -> Result<(u64, u64), Status> {
    if start_byte == 0 && end_byte == 0 {
        return Ok((0, file_size_bytes));
    }
    let range_len = end_byte.checked_sub(start_byte).ok_or_else(|| {
        Status::invalid_argument("end_byte must be greater than or equal to start_byte")
    })?;
    Ok((start_byte, range_len))
}

pub(crate) fn status_from_file_range_error(err: FormatError) -> Status {
    match err {
        FormatError::InvalidInput(message) => Status::invalid_argument(message),
        other => Status::internal(format!("read object range: {other}")),
    }
}

pub(crate) fn verify_loaded_tape_identity(
    drive: &mut DriveHandle,
    tape_uuid: &[u8; 16],
) -> Result<(), Status> {
    drive
        .rewind()
        .map_err(|err| Status::internal(format!("rewind before read: {err}")))?;
    let mut source = DriveHandleSource(drive);
    verify_tape_identity(&mut source, tape_uuid)
        .map_err(|err| Status::failed_precondition(format!("tape identity: {err}")))?;
    Ok(())
}

pub(crate) fn position_read_resume(
    index: &CatalogIndex,
    drive: &mut DriveHandle,
    target: &ReadResumeTarget,
) -> Result<u64, Status> {
    let request = file_range_read_request(
        index,
        &target.tape_uuid,
        target.object_id.as_str(),
        target.file_id.as_str(),
        0,
        0,
    )?;
    drive
        .rewind()
        .map_err(|err| Status::internal(format!("rewind before resume position: {err}")))?;
    let mut source = DriveHandleSource(drive);
    verify_and_position_read_resume_from_source(&mut source, request, target)
}

pub(crate) fn verify_and_position_read_resume_from_source(
    source: &mut dyn BlockSource,
    request: crate::read_core::PlaintextFileRangeReadRequest,
    target: &ReadResumeTarget,
) -> Result<u64, Status> {
    verify_tape_identity(source, &target.tape_uuid)
        .map_err(|err| Status::failed_precondition(format!("tape identity: {err}")))?;
    source
        .locate(0)
        .map_err(|err| Status::internal(format!("return to BOT after identity proof: {err}")))?;
    position_read_resume_from_source(source, request, target)
}

pub(crate) fn position_read_resume_from_source(
    source: &mut dyn BlockSource,
    request: crate::read_core::PlaintextFileRangeReadRequest,
    target: &ReadResumeTarget,
) -> Result<u64, Status> {
    let first_chunk_lba = request.first_chunk_lba.ok_or_else(|| {
        Status::failed_precondition("resume target file has no data-chunk boundary")
    })?;
    let block_size = u64::try_from(request.block_size)
        .map_err(|_| Status::internal("tape block size does not fit u64"))?;
    let catalog_boundary = first_chunk_lba
        .0
        .checked_mul(block_size)
        .ok_or_else(|| Status::internal("catalogued file boundary byte offset overflow"))?;
    if target.file_boundary_byte_offset != catalog_boundary {
        return Err(Status::invalid_argument(format!(
            "resume offset is not the catalogued file boundary: expected {catalog_boundary}, got {}",
            target.file_boundary_byte_offset
        )));
    }

    let tape_file_spacing = i64::try_from(request.tape_file_number)
        .map_err(|_| Status::invalid_argument("tape file number exceeds SPACE range"))?;
    let mut positioned = source
        .space(tape_file_spacing, SpaceKind::Filemarks)
        .map_err(|err| Status::internal(format!("space to resume object: {err}")))?
        .position_after;
    let skip_blocks = i64::try_from(first_chunk_lba.0)
        .map_err(|_| Status::invalid_argument("resume file boundary exceeds SPACE range"))?;
    if skip_blocks != 0 {
        positioned = source
            .space(skip_blocks, SpaceKind::Blocks)
            .map_err(|err| Status::internal(format!("space to resume file boundary: {err}")))?
            .position_after;
    }
    let proof = source
        .prove_read_position(positioned)
        .map_err(|err| Status::failed_precondition(format!("resume position proof: {err}")))?;
    if let Some(expected) = target.expected_position_lba {
        if proof.lba() != expected {
            return Err(Status::failed_precondition(format!(
                "resume position proof mismatch: expected LBA {expected}, observed {}",
                proof.lba()
            )));
        }
    }
    Ok(proof.lba())
}

pub(crate) fn read_session_proto(
    session_id: Uuid,
    tape_uuid: &TapeUuid,
    state: pb::read_session::State,
    opened_at_utc: &str,
    drive_element_address: u16,
    position_after_lba: Option<u64>,
    daemon_epoch: u64,
) -> pb::ReadSession {
    pb::ReadSession {
        session_id: session_id.as_bytes().to_vec(),
        // This projection is handed both values by its caller, so Some is the
        // truth here. The presence exists for callers further up that open a
        // session against a drive rather than a named volume.
        tape_uuid: Some(tape_uuid.to_vec()),
        drive_element_address: Some(u32::from(drive_element_address)),
        state: state as i32,
        opened_at: timestamp_from_rfc3339(opened_at_utc),
        position_proof: position_after_lba
            .map(|position_after_lba| pb::DevicePositionProof { position_after_lba }),
        daemon_epoch,
    }
}

pub(crate) struct WriteSessionProtoInput<'a> {
    pub(crate) session_id: Uuid,
    pub(crate) tape_uuid: &'a TapeUuid,
    pub(crate) target_kind: pb::write_session::TargetKind,
    pub(crate) state: pb::write_session::State,
    pub(crate) objects_committed: u64,
    pub(crate) bytes_committed: u64,
    pub(crate) opened_at_utc: &'a str,
    pub(crate) last_checkpoint_at_utc: Option<&'a str>,
    pub(crate) drive_element_address: u16,
    pub(crate) pending_batch: Option<&'a PendingCheckpointBatch>,
}

pub(crate) fn session_proto(input: WriteSessionProtoInput<'_>) -> pb::WriteSession {
    let checkpoint_deadline = input.pending_batch.map(|batch| {
        let remaining = batch.deadline.saturating_duration_since(Instant::now());
        let seconds = OffsetDateTime::now_utc()
            .unix_timestamp()
            .saturating_add(i64::try_from(remaining.as_secs()).unwrap_or(i64::MAX));
        prost_types::Timestamp { seconds, nanos: 0 }
    });
    pb::WriteSession {
        session_id: input.session_id.as_bytes().to_vec(),
        tape_uuid: Some(input.tape_uuid.to_vec()),
        drive_element_address: Some(u32::from(input.drive_element_address)),
        body_format: "rem-object-v1".to_string(),
        state: input.state as i32,
        objects_committed: input.objects_committed,
        bytes_committed: input.bytes_committed,
        opened_at: timestamp_from_rfc3339(input.opened_at_utc),
        last_checkpoint_at: input
            .last_checkpoint_at_utc
            .and_then(timestamp_from_rfc3339),
        target_kind: input.target_kind as i32,
        tape_sequence: vec![input.tape_uuid.to_vec()],
        current_tape_index: 0,
        pending_checkpoint_objects: input
            .pending_batch
            .map_or(0, |batch| batch.objects.len() as u64),
        pending_checkpoint_bytes: input.pending_batch.map_or(0, |batch| batch.logical_bytes),
        // `.map`, not `.map_or(0, ..)`: no pending batch means there is no
        // oldest pending object to have an age. Zero is what an object that
        // arrived this instant reports, so the old default made "nothing
        // waiting" and "something waiting, just now" identical -- and those
        // are opposite answers to "is this session behind on checkpoints?".
        oldest_pending_age_seconds: input
            .pending_batch
            .map(|batch| batch.opened_at.elapsed().as_secs()),
        checkpoint_deadline,
        checkpointed_objects: Vec::new(),
        committed_copies: Vec::new(),
    }
}

pub(crate) fn status_from_pool_write_error(err: PoolWriteError) -> Status {
    let message = err.to_string();
    match err {
        PoolWriteError::Select(select) => status_from_select_tape_error(select),
        PoolWriteError::State(state) => status_from_state_error(state),
        PoolWriteError::InvalidInput(_) => Status::invalid_argument(message),
        PoolWriteError::MissingTapeGeometry(_) => Status::failed_precondition(message),
        PoolWriteError::ParityAppendUnsupported { .. } => Status::failed_precondition(message),
        PoolWriteError::SelectedTapeInsufficientCapacity { .. } => {
            Status::failed_precondition(message)
        }
        PoolWriteError::TerminalCloseRequired { .. } => Status::resource_exhausted(message),
        PoolWriteError::ContentHashMismatch { .. } => Status::failed_precondition(message),
        PoolWriteError::WriteAdmissionConflict(_) => Status::aborted(message),
        PoolWriteError::ObjectWriteMedia(_) | PoolWriteError::TapeIdentity(_) => {
            Status::failed_precondition(message)
        }
        PoolWriteError::CallerObjectIdConflict { .. }
        | PoolWriteError::CallerObjectIdInputKindConflict { .. }
        | PoolWriteError::CallerObjectIdArchivePathConflict { .. }
        | PoolWriteError::CallerObjectIdRepresentationConflict { .. } => {
            Status::already_exists(message)
        }
        PoolWriteError::ReplayObjectInvalid { .. } => Status::internal(message),
        PoolWriteError::Streaming(streaming) => status_from_streaming_error(&streaming, message),
        PoolWriteError::Parity(parity) => status_from_parity_error(&parity, message),
        PoolWriteError::PhysicalUsedBytesOverflow { .. }
        | PoolWriteError::CheckpointReconciliation(_)
        | PoolWriteError::Io { .. }
        | PoolWriteError::TapeIo(_)
        | PoolWriteError::TransferWithSecondary { .. }
        | PoolWriteError::TimeFormat(_) => Status::internal(message),
    }
}

pub(crate) fn status_from_streaming_error(err: &StreamingError, message: String) -> Status {
    match err {
        StreamingError::InvalidInput(_) | StreamingError::InvalidXattrNamespacePrefix { .. } => {
            Status::invalid_argument(message)
        }
        StreamingError::Format(format) => status_from_format_error(format, message),
        StreamingError::Parity(parity) => status_from_parity_error(parity, message),
        StreamingError::Io { .. } => Status::internal(message),
    }
}

pub(crate) fn status_from_format_error(err: &FormatError, message: String) -> Status {
    match err {
        FormatError::InvalidInput(_) => Status::invalid_argument(message),
        _ => Status::internal(message),
    }
}

pub(crate) fn status_from_parity_error(err: &ParityError, message: String) -> Status {
    match err {
        ParityError::CapacityReserveExceeded { .. }
        | ParityError::ObjectTooLargeForEmptyTape { .. }
        | ParityError::BootstrapPayloadTooLarge { .. } => Status::resource_exhausted(message),
        _ => Status::internal(message),
    }
}

pub(crate) fn status_from_pinned_tape_error(err: crate::pool_write::PinnedTapeError) -> Status {
    use crate::pool_write::PinnedTapeError;
    let message = err.to_string();
    match err {
        PinnedTapeError::UnknownTape { .. } => Status::not_found(message),
        PinnedTapeError::NotADataTape { .. }
        | PinnedTapeError::PoolGuardMismatch { .. }
        | PinnedTapeError::NotWritable { .. }
        | PinnedTapeError::Fenced { .. }
        | PinnedTapeError::NotBatchEligible { .. } => Status::failed_precondition(message),
        PinnedTapeError::Select(err) => status_from_select_tape_error(err),
        PinnedTapeError::State(state) => status_from_state_error(state),
    }
}

pub(crate) fn status_from_select_tape_error(err: SelectTapeError) -> Status {
    let message = err.to_string();
    match err {
        SelectTapeError::UnknownPool { .. } => Status::invalid_argument(message),
        SelectTapeError::EmptyPool { .. }
        | SelectTapeError::NoWritableTapes { .. }
        | SelectTapeError::NoUnreservedWritableTapes { .. }
        | SelectTapeError::AmbiguousNeedsPolicy { .. } => Status::resource_exhausted(message),
        SelectTapeError::NoBatchedEligibleTapes { .. } => Status::failed_precondition(message),
        SelectTapeError::InvalidTapeGeometry { .. } => Status::failed_precondition(message),
        SelectTapeError::InvalidTapeUuid { .. } => Status::internal(message),
        SelectTapeError::State(state) => status_from_state_error(state),
    }
}

pub(crate) fn now_rfc3339() -> Result<String, time::error::Format> {
    OffsetDateTime::now_utc().format(&Rfc3339)
}
