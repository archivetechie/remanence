//! Canonical input validation, hashing, encryption preparation, and fixed-block emission.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use remanence_aead::{RecipientPublicKey, SealReport};
use remanence_format::{
    stream_rem_tar_object, write_encrypted_rem_object_from_readers,
    write_rem_tar_object_from_readers, FormatError, MetadataPreservation, RemTarEntrySink,
    RemTarEntryType, RemTarFileSpec, RemTarFileStream, RemTarObjectOptions, RemTarStreamEntry,
    FORMAT_ID, MANIFEST_PATH,
};
use remanence_library::{
    BlockSink, FileBlockSource, TapeIoError, TapePosition, VecBlockSink, WriteFilemarksOutcome,
    WriteOutcome,
};
use remanence_parity::{
    CommittedBundle, CommittedBundleKind, ObjectWriteSummary, ParityConfig, ParitySink,
    TapeFileEntry, TapeFileKind, TerminalTripleObjectReservation,
};
use remanence_state::{OBJECT_COPY_REPRESENTATION_ENCRYPTED, OBJECT_COPY_REPRESENTATION_PLAINTEXT};
use remanence_stream::{
    plan_prepared_object, prepare_regular_file, ObjectCatalogProjection, ObjectCopyProjection,
    PreparedFile, StreamingAuditEvent, StreamingCatalogProjection, StreamingError,
    StreamingObjectPlan, StreamingObjectWriteReport,
};
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

use super::capacity::{now_rfc3339, sha256_file, source_file_size, uuid_text};
use super::direct::{build_tape_bootstrap, write_tape_bootstrap};
use super::model::{
    NoParityAppendContext, PoolWriteDurability, PoolWriteError, PoolWriteRepresentation,
    SelectedTape, TapeUuid, WriteObjectInputKind, WriteObjectSource, WriteObjectToPoolRequest,
};
use super::no_parity::no_parity_file_catalog_projection;
use super::staging::{BlockSinkStats, ObjectDigestBlockSink};
use crate::bytes_to_hex;

pub(crate) struct PreparedPoolObject {
    pub(crate) content_sha256: [u8; 32],
    pub(crate) object_uuid: Uuid,
    pub(crate) write_timestamp: String,
    pub(crate) options: RemTarObjectOptions,
    pub(crate) files: Vec<PreparedFile>,
    pub(crate) plan: StreamingObjectPlan,
    pub(crate) source: PreparedPoolSource,
}

impl PreparedPoolObject {
    pub(crate) fn overlap_control(&self) -> Option<Arc<crate::append_ring::AppendRingControl>> {
        match &self.source {
            PreparedPoolSource::Paths | PreparedPoolSource::CanonicalPath(_) => None,
            PreparedPoolSource::Streamed { control, .. } => Some(Arc::clone(control)),
        }
    }
}

pub(crate) enum PreparedPoolSource {
    Paths,
    CanonicalPath(PathBuf),
    Streamed {
        reader: Arc<Mutex<Box<dyn Read + Send>>>,
        control: Arc<crate::append_ring::AppendRingControl>,
    },
}

pub(crate) struct PreparedPoolWrite {
    pub(crate) prepared: PreparedPoolObject,
    pub(crate) stored: PreparedStoredObject,
}

pub(crate) struct PreparedEncryptedPoolObject {
    pub(crate) plaintext_layout: remanence_format::RemTarObjectLayout,
    pub(crate) envelope: SealReport,
    pub(crate) sealed: Vec<u8>,
}

pub(crate) enum PreparedStoredObject {
    Plaintext,
    CanonicalPlaintext,
    Encrypted(Box<PreparedEncryptedPoolObject>),
}

impl PreparedStoredObject {
    pub(crate) fn projected_size_blocks(&self, prepared: &PreparedPoolObject) -> u64 {
        match self {
            Self::Plaintext => prepared.plan.layout.projected_size_blocks,
            Self::CanonicalPlaintext => prepared.plan.layout.projected_size_blocks,
            Self::Encrypted(encrypted) => encrypted.envelope.stored_size_blocks,
        }
    }

    pub(crate) fn representation_label(&self) -> &'static str {
        match self {
            Self::Plaintext | Self::CanonicalPlaintext => OBJECT_COPY_REPRESENTATION_PLAINTEXT,
            Self::Encrypted(_) => OBJECT_COPY_REPRESENTATION_ENCRYPTED,
        }
    }

    pub(crate) fn copy_representation(&self) -> CopyRepresentation {
        match self {
            Self::Plaintext | Self::CanonicalPlaintext => CopyRepresentation::plaintext(),
            Self::Encrypted(encrypted) => CopyRepresentation::encrypted(&encrypted.envelope),
        }
    }
}

pub(crate) fn stored_footprint_bytes(
    stored: &PreparedStoredObject,
    prepared: &PreparedPoolObject,
    selected_block_size: u32,
) -> Result<u64, PoolWriteError> {
    if prepared.options.chunk_size != selected_block_size as usize {
        return Err(PoolWriteError::InvalidInput(format!(
            "prepared chunk size {} does not match selected tape block size {selected_block_size}",
            prepared.options.chunk_size
        )));
    }
    stored
        .projected_size_blocks(prepared)
        .checked_mul(u64::from(selected_block_size))
        .ok_or_else(|| PoolWriteError::InvalidInput("stored object byte size overflow".to_string()))
}

#[derive(Clone)]
pub(crate) struct CopyRepresentation {
    pub(crate) representation: &'static str,
    pub(crate) recipient_epoch_ids: Option<Vec<String>>,
    pub(crate) recovery_recipient_epoch_ids: Option<Vec<[u8; 16]>>,
    pub(crate) metadata_frame_len: Option<u64>,
    pub(crate) key_frame_len: Option<u32>,
}

impl CopyRepresentation {
    pub(crate) fn plaintext() -> Self {
        Self {
            representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT,
            recipient_epoch_ids: None,
            recovery_recipient_epoch_ids: None,
            metadata_frame_len: None,
            key_frame_len: None,
        }
    }

    pub(crate) fn encrypted(envelope: &SealReport) -> Self {
        let recipient_epoch_ids = envelope
            .key_frame
            .slots
            .iter()
            .map(|slot| bytes_to_hex(&slot.recipient_epoch_id))
            .collect();
        let recovery_recipient_epoch_ids = envelope_recipient_epoch_ids(envelope);
        Self {
            representation: OBJECT_COPY_REPRESENTATION_ENCRYPTED,
            recipient_epoch_ids: Some(recipient_epoch_ids),
            recovery_recipient_epoch_ids: Some(recovery_recipient_epoch_ids),
            metadata_frame_len: Some(envelope.metadata_frame_len),
            key_frame_len: Some(envelope.header.key_frame_len),
        }
    }
}

pub(crate) fn prepared_payload_bytes(prepared: &PreparedPoolObject) -> u64 {
    prepared
        .files
        .iter()
        .fold(0u64, |acc, file| acc.saturating_add(file.spec.size_bytes))
}

pub(crate) fn parity_label(parity: &ParityConfig) -> &'static str {
    match parity {
        ParityConfig::Scheme(_) => "scheme",
        ParityConfig::None => "none",
    }
}

pub(crate) fn log_transfer_diagnostics(
    _request: &WriteObjectToPoolRequest,
    selected: &SelectedTape,
    prepared: &PreparedPoolObject,
    stored_projected_blocks: u64,
    write_filemarks_immed: bool,
    outcome: TransferDiagnosticOutcome<'_>,
) {
    let payload_bytes = prepared_payload_bytes(prepared);
    tracing::info!(
        target: "remanence_write_diag",
        phase = "transfer",
        pool_id = %selected.pool_id,
        tape_uuid = %uuid_text(selected.tape_uuid),
        parity = parity_label(&selected.parity_config),
        status = outcome.status,
        error = outcome.error.unwrap_or(""),
        payload_bytes,
        selected_block_size_bytes = selected.block_size,
        format_chunk_size_bytes = prepared.options.chunk_size,
        projected_object_blocks = stored_projected_blocks,
        sink_write_bytes = outcome.stats.block_write_bytes,
        block_write_calls = outcome.stats.block_write_calls,
        min_block_bytes = outcome.stats.min_block_bytes.unwrap_or(0),
        max_block_bytes = outcome.stats.max_block_bytes.unwrap_or(0),
        filemark_calls = outcome.stats.filemark_calls,
        filemarks = outcome.stats.filemarks,
        filemark_write_drain_ms = crate::diagnostics::duration_ms(
            outcome.stats.filemark_write_drain
        ),
        position_calls = outcome.stats.position_calls,
        early_warning = outcome.stats.early_warning,
        scsi_write_cdb = "WRITE6_FIXED_BATCH",
        write_batch_blocks = outcome.stats.write_batch_blocks,
        effective_batch_blocks = outcome.stats.effective_batch_blocks,
        position_check_bytes = outcome.stats.position_check_bytes,
        staging_ring_buffers = outcome.stats.staging_ring_buffers,
        staging_wait_samples = outcome.stats.staging_wait_samples,
        staging_wait_p50_us = outcome.stats.staging_wait_p50_us,
        staging_wait_p95_us = outcome.stats.staging_wait_p95_us,
        staging_wait_max_us = outcome.stats.staging_wait_max_us,
        staging_wait_mean_us = outcome.stats.staging_wait_mean_us,
        refill_samples = outcome.stats.refill_samples,
        refill_p50_us = outcome.stats.refill_p50_us,
        refill_p95_us = outcome.stats.refill_p95_us,
        refill_max_us = outcome.stats.refill_max_us,
        refill_mean_us = outcome.stats.refill_mean_us,
        gap_samples = outcome.stats.gap_samples,
        gap_p50_us = outcome.stats.gap_p50_us,
        gap_p95_us = outcome.stats.gap_p95_us,
        gap_max_us = outcome.stats.gap_max_us,
        gap_mean_us = outcome.stats.gap_mean_us,
        ioctl_samples = outcome.stats.ioctl_samples,
        ioctl_p50_us = outcome.stats.ioctl_p50_us,
        ioctl_p95_us = outcome.stats.ioctl_p95_us,
        ioctl_max_us = outcome.stats.ioctl_max_us,
        ioctl_mean_us = outcome.stats.ioctl_mean_us,
        first_60s_ioctl_samples = outcome.stats.first_60s_ioctl_samples,
        first_60s_ioctl_p50_us = outcome.stats.first_60s_ioctl_p50_us,
        first_60s_ioctl_p95_us = outcome.stats.first_60s_ioctl_p95_us,
        first_60s_ioctl_max_us = outcome.stats.first_60s_ioctl_max_us,
        first_60s_ioctl_mean_us = outcome.stats.first_60s_ioctl_mean_us,
        accounting_samples = outcome.stats.accounting_samples,
        accounting_p50_us = outcome.stats.accounting_p50_us,
        accounting_p95_us = outcome.stats.accounting_p95_us,
        accounting_max_us = outcome.stats.accounting_max_us,
        accounting_mean_us = outcome.stats.accounting_mean_us,
        cadence_us = outcome.stats.cadence_us,
        effective_feed_bytes_per_second = outcome.stats.effective_feed_bytes_per_second,
        time_to_first_ioctl_ms = outcome.stats.time_to_first_ioctl_ms,
        steady_reached = outcome.stats.steady_reached,
        time_to_steady_ms = outcome.stats.time_to_steady_ms,
        steady_window_seconds = outcome.stats.steady_window_seconds,
        steady_threshold_percent = outcome.stats.steady_threshold_percent,
        ramp_observation_seconds = outcome.stats.ramp_observation_seconds,
        write_filemarks_immed,
        elapsed_ms = crate::diagnostics::duration_ms(outcome.elapsed),
        throughput_mib_s = crate::diagnostics::mib_per_s(payload_bytes, outcome.elapsed),
        "remanence_write_diag",
    );
}

pub(crate) struct TransferDiagnosticOutcome<'a> {
    pub(crate) stats: BlockSinkStats,
    pub(crate) elapsed: Duration,
    pub(crate) status: &'static str,
    pub(crate) error: Option<&'a str>,
}

#[cfg(test)]
pub(crate) fn log_commit_diagnostics(
    _request: &WriteObjectToPoolRequest,
    selected: &SelectedTape,
    prepared: &PreparedPoolObject,
    elapsed: Duration,
    status: &str,
    error: Option<&str>,
) {
    let payload_bytes = prepared_payload_bytes(prepared);
    tracing::info!(
        target: "remanence_write_diag",
        phase = "commit",
        pool_id = %selected.pool_id,
        tape_uuid = %uuid_text(selected.tape_uuid),
        parity = parity_label(&selected.parity_config),
        status,
        error = error.unwrap_or(""),
        payload_bytes,
        elapsed_ms = crate::diagnostics::duration_ms(elapsed),
        throughput_mib_s = crate::diagnostics::mib_per_s(payload_bytes, elapsed),
        "remanence_write_diag",
    );
}

pub(crate) fn prepare_pool_object(
    request: &WriteObjectToPoolRequest,
    block_size: u32,
) -> Result<PreparedPoolObject, PoolWriteError> {
    validate_input_kind_guards(request)?;
    if request.input_kind == WriteObjectInputKind::CanonicalPlaintextRemObject {
        return prepare_canonical_plaintext_pool_object(request, block_size);
    }
    let content_sha256 = request.source.content_sha256()?;
    let object_uuid = Uuid::new_v4();
    let object_id = object_uuid.to_string();
    let write_timestamp = now_rfc3339()?;
    let mut options = RemTarObjectOptions::new(
        object_id,
        request.caller_object_id.clone(),
        write_timestamp.clone(),
        Uuid::new_v4().to_string(),
    );
    options.chunk_size = block_size as usize;
    let (files, source) = match &request.source {
        WriteObjectSource::Path(source_path) => (
            vec![prepare_regular_file(
                source_path,
                &request.archive_path,
                Uuid::new_v4().to_string(),
            )?],
            PreparedPoolSource::Paths,
        ),
        WriteObjectSource::Streamed(streamed) => {
            if !matches!(request.representation, PoolWriteRepresentation::Plaintext) {
                return Err(PoolWriteError::InvalidInput(
                    "streamed pool sources support plaintext representation only".to_string(),
                ));
            }
            let archive_path = request.archive_path.to_str().ok_or_else(|| {
                PoolWriteError::InvalidInput("streamed archive path must be UTF-8".to_string())
            })?;
            let mut spec = RemTarFileSpec::new(
                archive_path,
                Uuid::new_v4().to_string(),
                streamed.size_bytes,
                streamed.content_sha256,
            );
            spec.mtime = None;
            spec.executable = Some(false);
            (
                vec![PreparedFile {
                    source_path: PathBuf::new(),
                    spec,
                }],
                PreparedPoolSource::Streamed {
                    reader: Arc::clone(&streamed.reader),
                    control: Arc::clone(&streamed.control),
                },
            )
        }
    };
    let plan = plan_prepared_object(&options, &files)?;
    Ok(PreparedPoolObject {
        content_sha256,
        object_uuid,
        write_timestamp,
        options,
        files,
        plan,
        source,
    })
}

pub(crate) fn validate_input_kind_guards(
    request: &WriteObjectToPoolRequest,
) -> Result<(), PoolWriteError> {
    match (request.input_kind, request.expected_object_id) {
        (WriteObjectInputKind::LogicalFile, Some(_)) => Err(PoolWriteError::InvalidInput(
            "expected_object_id is valid only for canonical plaintext REM object ingestion"
                .to_string(),
        )),
        (WriteObjectInputKind::CanonicalPlaintextRemObject, None) => {
            Err(PoolWriteError::InvalidInput(
                "canonical plaintext REM object ingestion requires expected_object_id".to_string(),
            ))
        }
        _ => Ok(()),
    }
}

#[derive(Default)]
pub(crate) struct ValidateCanonicalEntrySink;

impl RemTarEntrySink for ValidateCanonicalEntrySink {
    fn begin_file(&mut self, _entry: &RemTarStreamEntry) -> Result<(), FormatError> {
        Ok(())
    }

    fn write_file_data(&mut self, _bytes: &[u8]) -> Result<(), FormatError> {
        Ok(())
    }

    fn end_file(&mut self, _entry: &RemTarStreamEntry) -> Result<(), FormatError> {
        Ok(())
    }
}

pub(crate) struct CanonicalDigestSink {
    pub(crate) hasher: Sha256,
    pub(crate) block_size: usize,
    pub(crate) next_lba: u64,
}

impl CanonicalDigestSink {
    pub(crate) fn new(block_size: usize) -> Self {
        Self {
            hasher: Sha256::new(),
            block_size,
            next_lba: 0,
        }
    }

    pub(crate) fn finish(self) -> [u8; 32] {
        self.hasher.finalize().into()
    }

    pub(crate) fn position_value(&self) -> TapePosition {
        TapePosition {
            lba: self.next_lba,
            partition: 0,
            beginning_of_partition: self.next_lba == 0,
            end_of_partition: false,
            block_position_end_of_warning: false,
        }
    }
}

impl BlockSink for CanonicalDigestSink {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        if buf.len() != self.block_size {
            return Err(TapeIoError::OperationFailed(format!(
                "canonical regeneration emitted {} bytes, expected one {}-byte block",
                buf.len(),
                self.block_size
            )));
        }
        self.hasher.update(buf);
        self.next_lba = self.next_lba.checked_add(1).ok_or_else(|| {
            TapeIoError::OperationFailed("canonical regeneration LBA overflow".to_string())
        })?;
        Ok(WriteOutcome::from_device_position(
            buf.len() as u32,
            false,
            false,
            self.position_value(),
        ))
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        if count != 0 {
            return Err(TapeIoError::OperationFailed(
                "canonical regeneration does not accept filemarks".to_string(),
            ));
        }
        Ok(WriteFilemarksOutcome::from_device_position(
            false,
            false,
            self.position_value(),
        ))
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        Ok(self.position_value())
    }
}

pub(crate) fn canonical_admission_format_error(error: FormatError) -> PoolWriteError {
    if matches!(error, FormatError::SourceIo { .. } | FormatError::TapeIo(_)) {
        PoolWriteError::Streaming(StreamingError::Format(error))
    } else {
        PoolWriteError::InvalidInput(format!(
            "canonical plaintext REM object is malformed: {error}"
        ))
    }
}

pub(crate) fn regenerate_canonical_plaintext_digest(
    source_path: &Path,
    options: &RemTarObjectOptions,
    files: &[PreparedFile],
    entries: &[RemTarStreamEntry],
) -> Result<(remanence_format::RemTarObjectLayout, [u8; 32]), PoolWriteError> {
    let payload_entries = entries
        .iter()
        .filter(|entry| entry.path != MANIFEST_PATH)
        .collect::<Vec<_>>();
    if payload_entries.len() != files.len() {
        return Err(PoolWriteError::InvalidInput(
            "canonical plaintext REM member count changed during regeneration".to_string(),
        ));
    }
    let shared_source = Arc::new(Mutex::new(File::open(source_path).map_err(|source| {
        PoolWriteError::Io {
            context: "reopen canonical plaintext REM object for canonical regeneration",
            path: source_path.to_path_buf(),
            source,
        }
    })?));
    let mut readers = Vec::<Box<dyn Read + Send>>::with_capacity(files.len());
    for entry in payload_entries {
        readers.push(Box::new(SharedFileRangeReader::new(
            Arc::clone(&shared_source),
            entry.data_offset,
            entry.size_bytes,
        )));
    }
    let mut streams = Vec::with_capacity(files.len());
    for (file, reader) in files.iter().zip(readers.iter_mut()) {
        streams.push(RemTarFileStream::new(file.spec.clone(), reader.as_mut()));
    }
    let mut sink = CanonicalDigestSink::new(options.chunk_size);
    let layout = write_rem_tar_object_from_readers(&mut sink, options, &mut streams)
        .map_err(StreamingError::from)?;
    Ok((layout, sink.finish()))
}

/// Lazy range reader over one shared canonical spool descriptor.
///
/// The deterministic writer owns one reader per member, but retaining one
/// `File` per member would turn the process descriptor limit into an archive
/// member-count ceiling. Each range keeps only its cursor; reads serialize the
/// seek/read pair against the single daemon-owned spool descriptor.
pub(crate) struct SharedFileRangeReader {
    pub(crate) source: Arc<Mutex<File>>,
    pub(crate) cursor: u64,
    pub(crate) remaining: u64,
}

impl SharedFileRangeReader {
    pub(crate) fn new(source: Arc<Mutex<File>>, start: u64, len: u64) -> Self {
        Self {
            source,
            cursor: start,
            remaining: len,
        }
    }
}

impl Read for SharedFileRangeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 || buf.is_empty() {
            return Ok(0);
        }
        let wanted = usize::try_from(self.remaining.min(buf.len() as u64)).unwrap_or(buf.len());
        let mut source = self
            .source
            .lock()
            .map_err(|_| io::Error::other("canonical spool range-reader lock poisoned"))?;
        source.seek(SeekFrom::Start(self.cursor))?;
        let read = source.read(&mut buf[..wanted])?;
        let read_u64 = read as u64;
        self.cursor = self.cursor.checked_add(read_u64).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "canonical spool range-reader cursor overflow",
            )
        })?;
        self.remaining = self.remaining.checked_sub(read_u64).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "canonical spool range-reader remaining underflow",
            )
        })?;
        Ok(read)
    }
}

pub(crate) fn prepare_canonical_plaintext_pool_object(
    request: &WriteObjectToPoolRequest,
    block_size: u32,
) -> Result<PreparedPoolObject, PoolWriteError> {
    if !matches!(request.representation, PoolWriteRepresentation::Plaintext) {
        return Err(PoolWriteError::InvalidInput(
            "canonical plaintext REM objects cannot request envelope encryption".to_string(),
        ));
    }
    let source_path = match &request.source {
        WriteObjectSource::Path(path) => path,
        WriteObjectSource::Streamed(_) => {
            return Err(PoolWriteError::InvalidInput(
                "canonical plaintext REM objects require a completed spool".to_string(),
            ));
        }
    };
    let source_size = source_file_size(source_path)?;
    if source_size == 0 || source_size % u64::from(block_size) != 0 {
        return Err(PoolWriteError::InvalidInput(format!(
            "canonical plaintext REM object size {source_size} is not a nonzero multiple of selected block size {block_size}"
        )));
    }
    let block_count = source_size / u64::from(block_size);
    let mut source = FileBlockSource::open(source_path, block_size as usize)?;
    let mut entry_sink = ValidateCanonicalEntrySink;
    let report = stream_rem_tar_object(
        &mut source,
        block_size as usize,
        block_count,
        &mut entry_sink,
    )
    .map_err(canonical_admission_format_error)?;
    if !report.warnings.is_empty()
        || !report.digest_mismatches.is_empty()
        || report.manifest_cbor.is_none()
    {
        return Err(PoolWriteError::InvalidInput(
            "canonical plaintext REM object requires a valid final manifest and no integrity warnings"
                .to_string(),
        ));
    }

    let required_global = |key: &str| {
        report
            .global_pax
            .get(key)
            .filter(|value| !value.is_empty())
            .cloned()
            .ok_or_else(|| {
                PoolWriteError::InvalidInput(format!(
                    "canonical plaintext REM object is missing {key}"
                ))
            })
    };
    let object_id = required_global("REMANENCE.object_id")?;
    let object_uuid = Uuid::parse_str(&object_id).map_err(|error| {
        PoolWriteError::InvalidInput(format!(
            "canonical plaintext REM object_id must be a UUID: {error}"
        ))
    })?;
    if object_uuid.to_string() != object_id {
        return Err(PoolWriteError::InvalidInput(
            "canonical plaintext REM object_id must be canonical lowercase UUID text".to_string(),
        ));
    }
    if let Some(expected_object_id) = request.expected_object_id {
        if object_uuid.as_bytes() != &expected_object_id {
            return Err(PoolWriteError::InvalidInput(format!(
                "canonical plaintext REM object_id mismatch: embedded={}, expected={}",
                object_uuid,
                Uuid::from_bytes(expected_object_id)
            )));
        }
    }
    let embedded_caller_object_id = required_global("REMANENCE.caller_object_id")?;
    if embedded_caller_object_id != request.caller_object_id {
        return Err(PoolWriteError::InvalidInput(format!(
            "canonical plaintext REM caller_object_id mismatch: embedded={embedded_caller_object_id:?}, requested={:?}",
            request.caller_object_id
        )));
    }
    let write_timestamp = required_global("REMANENCE.write_timestamp")?;
    OffsetDateTime::parse(&write_timestamp, &Rfc3339).map_err(|error| {
        PoolWriteError::InvalidInput(format!(
            "canonical plaintext REM write_timestamp is invalid: {error}"
        ))
    })?;
    let metadata_preservation = match required_global("REMANENCE.metadata_preservation")?.as_str() {
        "minimal" => MetadataPreservation::Minimal,
        "archival" => MetadataPreservation::Archival,
        "full" => MetadataPreservation::Full,
        other => {
            return Err(PoolWriteError::InvalidInput(format!(
                "unsupported canonical plaintext REM metadata_preservation {other:?}"
            )));
        }
    };

    let manifest_entry = report
        .entries
        .iter()
        .find(|entry| entry.path == MANIFEST_PATH)
        .ok_or_else(|| {
            PoolWriteError::InvalidInput(
                "canonical plaintext REM object has no manifest entry".to_string(),
            )
        })?;
    let manifest_file_id = required_entry_text(manifest_entry, "REMANENCE.file_id")?;
    let mut options = RemTarObjectOptions::new(
        object_id,
        embedded_caller_object_id,
        write_timestamp.clone(),
        manifest_file_id,
    );
    options.chunk_size = block_size as usize;
    options.metadata_preservation = metadata_preservation;
    options.extensions = report.object_extensions.clone();

    let mut files = Vec::with_capacity(report.entries.len().saturating_sub(1));
    for entry in report
        .entries
        .iter()
        .filter(|entry| entry.path != MANIFEST_PATH)
    {
        if entry.entry_type != RemTarEntryType::Regular {
            return Err(PoolWriteError::InvalidInput(format!(
                "canonical daemon ingest currently requires regular payload members; {:?} is {:?}",
                entry.path, entry.entry_type
            )));
        }
        let file_id = required_entry_text(entry, "REMANENCE.file_id")?;
        let file_sha256 =
            parse_canonical_sha256(required_entry_text(entry, "REMANENCE.file_sha256")?)?;
        let executable = entry
            .pax_records
            .get("REMANENCE.executable")
            .map(|value| {
                value.parse::<bool>().map_err(|_| {
                    PoolWriteError::InvalidInput(format!(
                        "canonical plaintext REM executable flag for {:?} is invalid",
                        entry.path
                    ))
                })
            })
            .transpose()?;
        files.push(PreparedFile {
            source_path: PathBuf::new(),
            spec: RemTarFileSpec {
                entry_type: entry.entry_type,
                path: entry.path.clone(),
                file_id,
                size_bytes: entry.size_bytes,
                file_sha256: Some(file_sha256),
                link_target: entry.link_target.clone(),
                xattrs: entry.xattrs.clone(),
                extensions: entry.extensions.clone(),
                mtime: entry.pax_records.get("mtime").cloned(),
                executable,
            },
        });
    }
    if files.is_empty() {
        return Err(PoolWriteError::InvalidInput(
            "canonical plaintext REM object has no payload members".to_string(),
        ));
    }
    let plan = plan_prepared_object(&options, &files)?;
    let manifest_cbor = report
        .manifest_cbor
        .as_ref()
        .expect("manifest presence checked above");
    if plan.layout.projected_size_blocks != block_count
        || plan.layout.total_size_bytes != source_size
        || plan.layout.manifest_cbor != *manifest_cbor
    {
        return Err(PoolWriteError::InvalidInput(
            "canonical plaintext REM bytes do not match the deterministic layout plan".to_string(),
        ));
    }
    let content_sha256 = sha256_file(source_path)?;
    let (_regenerated_layout, regenerated_sha256) =
        regenerate_canonical_plaintext_digest(source_path, &options, &files, &report.entries)?;
    if regenerated_sha256 != content_sha256 {
        return Err(PoolWriteError::InvalidInput(
            "canonical plaintext REM bytes differ from the deterministic writer output".to_string(),
        ));
    }
    Ok(PreparedPoolObject {
        content_sha256,
        object_uuid,
        write_timestamp,
        options,
        files,
        plan,
        source: PreparedPoolSource::CanonicalPath(source_path.clone()),
    })
}

pub(crate) fn required_entry_text(
    entry: &RemTarStreamEntry,
    key: &str,
) -> Result<String, PoolWriteError> {
    entry
        .pax_records
        .get(key)
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| {
            PoolWriteError::InvalidInput(format!(
                "canonical plaintext REM entry {:?} is missing {key}",
                entry.path
            ))
        })
}

pub(crate) fn parse_canonical_sha256(value: String) -> Result<[u8; 32], PoolWriteError> {
    if value.len() != 64
        || value
            .bytes()
            .any(|byte| !byte.is_ascii_digit() && !(b'a'..=b'f').contains(&byte))
    {
        return Err(PoolWriteError::InvalidInput(
            "canonical plaintext REM file digest must be 64 lowercase hex digits".to_string(),
        ));
    }
    let mut digest = [0u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let text = std::str::from_utf8(pair).expect("validated digest bytes are ASCII");
        digest[index] = u8::from_str_radix(text, 16).map_err(|_| {
            PoolWriteError::InvalidInput(
                "canonical plaintext REM file digest is invalid".to_string(),
            )
        })?;
    }
    Ok(digest)
}

pub(crate) fn prepare_stored_object(
    prepared: &PreparedPoolObject,
    representation: &PoolWriteRepresentation,
) -> Result<PreparedStoredObject, PoolWriteError> {
    match representation {
        PoolWriteRepresentation::Plaintext => match &prepared.source {
            PreparedPoolSource::CanonicalPath(_) => Ok(PreparedStoredObject::CanonicalPlaintext),
            _ => Ok(PreparedStoredObject::Plaintext),
        },
        PoolWriteRepresentation::Encrypted { recipients } => Ok(PreparedStoredObject::Encrypted(
            Box::new(seal_prepared_object(prepared, recipients)?),
        )),
    }
}

pub(crate) fn seal_prepared_object(
    prepared: &PreparedPoolObject,
    recipients: &[RecipientPublicKey],
) -> Result<PreparedEncryptedPoolObject, PoolWriteError> {
    let mut encrypted_sink = VecBlockSink::new();
    let mut readers = open_prepared_readers(prepared)?;
    let mut streams = Vec::with_capacity(prepared.files.len());
    for (file, reader) in prepared.files.iter().zip(readers.iter_mut()) {
        streams.push(RemTarFileStream::new(file.spec.clone(), reader));
    }
    let report = write_encrypted_rem_object_from_readers(
        &mut encrypted_sink,
        &prepared.options,
        &mut streams,
        recipients,
    )
    .map_err(StreamingError::from)?;
    let plaintext_layout = report.plaintext_layout;
    if plaintext_layout.projected_size_blocks != prepared.plan.layout.projected_size_blocks {
        return Err(PoolWriteError::InvalidInput(
            "sealed plaintext layout differs from pre-admission plan".to_string(),
        ));
    }
    let envelope = report.envelope;
    let sealed = flatten_blocks(encrypted_sink.blocks, prepared.options.chunk_size)?;
    let block_count = u64::try_from(sealed.len() / prepared.options.chunk_size).map_err(|_| {
        PoolWriteError::InvalidInput("sealed REM-OBJECT block count overflow".to_string())
    })?;
    if sealed.len() % prepared.options.chunk_size != 0 || block_count != envelope.stored_size_blocks
    {
        return Err(PoolWriteError::InvalidInput(
            "sealed REM-OBJECT bytes do not match envelope block count".to_string(),
        ));
    }
    Ok(PreparedEncryptedPoolObject {
        plaintext_layout,
        envelope,
        sealed,
    })
}

pub(crate) fn flatten_blocks(
    blocks: Vec<Vec<u8>>,
    block_size: usize,
) -> Result<Vec<u8>, PoolWriteError> {
    let capacity = blocks
        .len()
        .checked_mul(block_size)
        .ok_or_else(|| PoolWriteError::InvalidInput("object byte length overflow".to_string()))?;
    let mut out = Vec::with_capacity(capacity);
    for block in blocks {
        if block.len() != block_size {
            return Err(PoolWriteError::InvalidInput(format!(
                "REM-OBJECT block length {} does not match chunk size {block_size}",
                block.len()
            )));
        }
        out.extend_from_slice(&block);
    }
    Ok(out)
}

pub(crate) fn envelope_recipient_epoch_ids(envelope: &SealReport) -> Vec<[u8; 16]> {
    envelope
        .key_frame
        .slots
        .iter()
        .map(|slot| slot.recipient_epoch_id)
        .collect()
}

pub(crate) fn write_fixed_blocks(
    sink: &mut dyn BlockSink,
    block_size: usize,
    bytes: &[u8],
) -> Result<u64, PoolWriteError> {
    if bytes.len() % block_size != 0 {
        return Err(PoolWriteError::InvalidInput(
            "stored REM-OBJECT bytes are not block aligned".to_string(),
        ));
    }
    let mut blocks = 0u64;
    for block in bytes.chunks_exact(block_size) {
        let outcome = sink.write_block(block)?;
        let expected_bytes = u64::try_from(block.len()).map_err(|_| {
            PoolWriteError::InvalidInput("fixed block length does not fit u64".to_string())
        })?;
        let written_bytes = u64::from(outcome.bytes_written);
        if written_bytes != expected_bytes || outcome.end_of_medium {
            return Err(PoolWriteError::TapeIo(
                TapeIoError::PartialBatchUncommittable {
                    requested_records: 1,
                    written_records: u32::from(written_bytes >= expected_bytes),
                    requested_bytes: expected_bytes,
                    written_bytes,
                    end_of_medium: outcome.end_of_medium,
                    sense: None,
                },
            ));
        }
        blocks = blocks
            .checked_add(1)
            .ok_or_else(|| PoolWriteError::InvalidInput("block count overflow".to_string()))?;
    }
    Ok(blocks)
}

#[cfg(test)]
pub(crate) fn position_no_parity_append(
    sink: &mut dyn BlockSink,
) -> Result<TapePosition, PoolWriteError> {
    sink.space_to_end_of_data().map_err(PoolWriteError::from)
}

pub(crate) fn position_no_parity_append_at_checkpoint(
    sink: &mut dyn BlockSink,
    journal_eod_lba: u64,
) -> Result<(), PoolWriteError> {
    let located = sink.locate(journal_eod_lba)?;
    if located.partition != 0 || located.lba != journal_eod_lba {
        return Err(PoolWriteError::TapeIo(TapeIoError::OperationFailed(
            format!(
                "checkpoint recovery LOCATE mismatch: expected partition 0 lba {journal_eod_lba}, observed partition {} lba {}",
                located.partition, located.lba
            ),
        )));
    }
    prove_no_parity_append_boundary(sink, journal_eod_lba)
}

pub(crate) fn prove_no_parity_append_boundary(
    sink: &mut dyn BlockSink,
    expected_lba: u64,
) -> Result<(), PoolWriteError> {
    let observed = sink.position()?;
    if observed.partition != 0 || observed.lba != expected_lba {
        return Err(PoolWriteError::TapeIo(TapeIoError::OperationFailed(
            format!(
                "checkpoint append position drift: expected partition 0 lba {expected_lba}, observed partition {} lba {}",
                observed.partition, observed.lba
            ),
        )));
    }
    Ok(())
}

pub(crate) fn write_object_delimiter(
    sink: &mut dyn BlockSink,
    durability: &PoolWriteDurability,
    append: NoParityAppendContext,
    object_blocks: u64,
) -> Result<WriteFilemarksOutcome, PoolWriteError> {
    match durability {
        #[cfg(test)]
        PoolWriteDurability::PerObject => Ok(sink.write_filemarks(1)?),
        PoolWriteDurability::Batched(_) => {
            sink.write_filemarks_immediate(1)?;
            // The object occupies [start, start + object_blocks); its
            // trailing filemark sits at start + object_blocks, and the
            // position after that filemark is one more.
            let position_after_lba = append
                .object_start_lba()?
                .checked_add(object_blocks)
                .and_then(|lba| lba.checked_add(1))
                .ok_or_else(|| {
                    PoolWriteError::InvalidInput(
                        "provisional delimiter position overflows u64".to_string(),
                    )
                })?;
            Ok(WriteFilemarksOutcome::from_computed_position(
                TapePosition {
                    lba: position_after_lba,
                    partition: 0,
                    beginning_of_partition: false,
                    end_of_partition: false,
                    block_position_end_of_warning: false,
                },
            ))
        }
    }
}

pub(crate) fn write_no_parity_bootstrap(
    sink: &mut dyn BlockSink,
    tape_uuid: TapeUuid,
    block_size: u32,
    written_at: &str,
) -> Result<(), PoolWriteError> {
    let payload = build_tape_bootstrap(
        tape_uuid,
        block_size,
        ParityConfig::None,
        written_at.to_string(),
        env!("CARGO_PKG_VERSION").to_string(),
    );
    write_tape_bootstrap(sink, &payload)
}

pub(crate) struct SharedStreamReader(Arc<Mutex<Box<dyn Read + Send>>>);

impl Read for SharedStreamReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .read(output)
    }
}

pub(crate) fn open_prepared_readers(
    prepared: &PreparedPoolObject,
) -> Result<Vec<Box<dyn Read + Send>>, PoolWriteError> {
    match &prepared.source {
        PreparedPoolSource::Paths => prepared
            .files
            .iter()
            .map(|file| {
                File::open(&file.source_path)
                    .map(|reader| Box::new(reader) as Box<dyn Read + Send>)
                    .map_err(|source| PoolWriteError::Io {
                        context: "open source file for streaming",
                        path: file.source_path.clone(),
                        source,
                    })
            })
            .collect(),
        PreparedPoolSource::CanonicalPath(_) => Err(PoolWriteError::InvalidInput(
            "canonical plaintext REM object must use verbatim block streaming".to_string(),
        )),
        PreparedPoolSource::Streamed { reader, .. } => {
            Ok(vec![Box::new(SharedStreamReader(Arc::clone(reader)))])
        }
    }
}

pub(crate) fn write_canonical_plaintext_blocks(
    sink: &mut dyn BlockSink,
    prepared: &PreparedPoolObject,
) -> Result<[u8; 32], PoolWriteError> {
    let path = match &prepared.source {
        PreparedPoolSource::CanonicalPath(path) => path,
        _ => {
            return Err(PoolWriteError::InvalidInput(
                "verbatim REM object write is missing its canonical source".to_string(),
            ));
        }
    };
    let mut reader = BufReader::new(File::open(path).map_err(|source| PoolWriteError::Io {
        context: "reopen canonical plaintext REM object for tape write",
        path: path.clone(),
        source,
    })?);
    let mut hashing = ObjectDigestBlockSink::new(sink);
    let mut block = vec![0u8; prepared.options.chunk_size];
    for _ in 0..prepared.plan.layout.projected_size_blocks {
        reader
            .read_exact(&mut block)
            .map_err(|source| PoolWriteError::Io {
                context: "read canonical plaintext REM object block",
                path: path.clone(),
                source,
            })?;
        let outcome = hashing.write_block(&block)?;
        if usize::try_from(outcome.bytes_written).ok() != Some(block.len()) || outcome.end_of_medium
        {
            return Err(PoolWriteError::InvalidInput(
                "canonical plaintext REM object suffered an incomplete tape block write"
                    .to_string(),
            ));
        }
    }
    let mut extra = [0u8; 1];
    if reader
        .read(&mut extra)
        .map_err(|source| PoolWriteError::Io {
            context: "verify canonical plaintext REM object EOF",
            path: path.clone(),
            source,
        })?
        != 0
    {
        return Err(PoolWriteError::InvalidInput(
            "canonical plaintext REM object grew after admission".to_string(),
        ));
    }
    let digest = hashing.finish_digest();
    if digest != prepared.content_sha256 {
        return Err(PoolWriteError::ContentHashMismatch {
            expected: bytes_to_hex(&prepared.content_sha256),
            actual: bytes_to_hex(&digest),
        });
    }
    Ok(digest)
}

pub(crate) fn no_parity_write_report(
    tape_uuid: TapeUuid,
    prepared: &PreparedPoolObject,
    layout: remanence_format::RemTarObjectLayout,
    object_digest: [u8; 32],
    filemark_outcome: remanence_library::WriteFilemarksOutcome,
    append: NoParityAppendContext,
) -> Result<StreamingObjectWriteReport, PoolWriteError> {
    if layout.projected_size_blocks != prepared.plan.layout.projected_size_blocks {
        return Err(PoolWriteError::InvalidInput(
            "emitted no-parity layout differs from pre-admission plan".to_string(),
        ));
    }
    if prepared.files.len() != layout.files.len() {
        return Err(PoolWriteError::InvalidInput(
            "prepared file count does not match emitted no-parity layout".to_string(),
        ));
    }
    let logical_size_bytes = layout.files.iter().try_fold(0u64, |acc, file| {
        acc.checked_add(file.size_bytes)
            .ok_or_else(|| PoolWriteError::InvalidInput("logical size overflow".to_string()))
    })?;
    let files = layout
        .files
        .iter()
        .zip(prepared.files.iter())
        .map(|(file, prepared_file)| {
            no_parity_file_catalog_projection(&prepared.options.object_id, file, prepared_file)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let object_start_lba = append.object_start_lba()?;
    let object_close = ObjectWriteSummary {
        tape_file_number: append.tape_file_number,
        first_parity_data_ordinal: 0,
        projected_size_blocks: prepared.plan.layout.projected_size_blocks,
        data_block_count: layout.projected_size_blocks,
        filemark_outcome,
        sidecars_emitted: Vec::new(),
        highest_protected_ordinal: 0,
        physical_start_lba: Some(object_start_lba),
    };
    let object = ObjectCatalogProjection {
        object_id: prepared.options.object_id.clone(),
        caller_object_id: prepared.options.caller_object_id.clone(),
        body_format: FORMAT_ID.to_string(),
        logical_size_bytes,
        manifest_sha256: layout.manifest_sha256,
    };
    let object_copy = ObjectCopyProjection {
        object_id: prepared.options.object_id.clone(),
        tape_uuid,
        tape_file_number: object_close.tape_file_number,
        first_parity_data_ordinal: None,
        data_block_count: object_close.data_block_count,
        protected_until_ordinal: None,
        parity_state: None,
        plaintext_digest: object_digest,
        stored_digest: object_digest,
    };
    let mut tape_file_entries = Vec::with_capacity(if append.fresh_tape { 2 } else { 1 });
    if append.fresh_tape {
        tape_file_entries.push(TapeFileEntry {
            tape_file_number: 0,
            kind: TapeFileKind::Bootstrap,
            block_count: 1,
            // A fresh tape's bootstrap prefix starts at BOT by definition.
            physical_start_hint: Some(0),
            object_id: None,
            first_parity_data_ordinal: None,
            epoch_id: None,
            protected_ordinal_start: None,
            protected_ordinal_end_exclusive: None,
            canonical_metadata_hash: None,
            object_recovery_row: None,
        });
    }
    tape_file_entries.push(TapeFileEntry {
        tape_file_number: object_close.tape_file_number,
        kind: TapeFileKind::Object,
        block_count: layout.projected_size_blocks,
        physical_start_hint: Some(object_start_lba),
        object_id: Some(prepared.options.object_id.clone()),
        first_parity_data_ordinal: None,
        epoch_id: None,
        protected_ordinal_start: None,
        protected_ordinal_end_exclusive: None,
        canonical_metadata_hash: None,
        object_recovery_row: None,
    });
    let tape_file_bundle = CommittedBundle {
        kind: CommittedBundleKind::Object,
        entries: tape_file_entries,
        highest_protected_ordinal: 0,
        total_committed_ordinals: append
            .object_total_committed_ordinals(layout.projected_size_blocks)?,
    };
    let catalog = StreamingCatalogProjection {
        object,
        files,
        object_copy,
        tape_file_bundle,
    };
    let audit_events = vec![StreamingAuditEvent {
        kind: "streaming_object_committed_no_parity",
        object_id: prepared.options.object_id.clone(),
        summary: format!(
            "committed no-parity object {} to tape file {} ({} payload files, {} object blocks)",
            prepared.options.object_id,
            object_close.tape_file_number,
            prepared.files.len(),
            object_close.data_block_count
        ),
    }];
    Ok(StreamingObjectWriteReport {
        layout,
        object_close,
        catalog,
        audit_events,
    })
}

pub(crate) fn write_canonical_plaintext_object_to_parity(
    parity: &mut ParitySink<'_>,
    tape_uuid: TapeUuid,
    prepared: &PreparedPoolObject,
    capacity: TerminalTripleObjectReservation,
) -> Result<StreamingObjectWriteReport, PoolWriteError> {
    let opened = parity.begin_object_with_terminal_triple_reservation(capacity)?;
    let object_digest = write_canonical_plaintext_blocks(parity, prepared)?;
    let object_close = parity.finish_object()?;
    if opened.0 != object_close.tape_file_number
        || object_close.data_block_count != prepared.plan.layout.projected_size_blocks
    {
        return Err(PoolWriteError::InvalidInput(
            "canonical plaintext REM parity close does not match admitted geometry".to_string(),
        ));
    }
    parity_plaintext_write_report(tape_uuid, prepared, object_digest, object_close)
}

pub(crate) fn parity_plaintext_write_report(
    tape_uuid: TapeUuid,
    prepared: &PreparedPoolObject,
    object_digest: [u8; 32],
    object_close: ObjectWriteSummary,
) -> Result<StreamingObjectWriteReport, PoolWriteError> {
    let layout = prepared.plan.layout.clone();
    if prepared.files.len() != layout.files.len() {
        return Err(PoolWriteError::InvalidInput(
            "prepared file count does not match canonical plaintext REM layout".to_string(),
        ));
    }
    let logical_size_bytes = layout.files.iter().try_fold(0u64, |acc, file| {
        acc.checked_add(file.size_bytes)
            .ok_or_else(|| PoolWriteError::InvalidInput("logical size overflow".to_string()))
    })?;
    let files = layout
        .files
        .iter()
        .zip(prepared.files.iter())
        .map(|(file, prepared_file)| {
            no_parity_file_catalog_projection(&prepared.options.object_id, file, prepared_file)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let object = ObjectCatalogProjection {
        object_id: prepared.options.object_id.clone(),
        caller_object_id: prepared.options.caller_object_id.clone(),
        body_format: FORMAT_ID.to_string(),
        logical_size_bytes,
        manifest_sha256: layout.manifest_sha256,
    };
    let parity_state = remanence_parity::ObjectParityState::from_ordinals(
        object_close.first_parity_data_ordinal,
        object_close.data_block_count,
        object_close.highest_protected_ordinal,
    )?;
    let object_copy = ObjectCopyProjection {
        object_id: prepared.options.object_id.clone(),
        tape_uuid,
        tape_file_number: object_close.tape_file_number,
        first_parity_data_ordinal: Some(object_close.first_parity_data_ordinal),
        data_block_count: object_close.data_block_count,
        protected_until_ordinal: Some(object_close.highest_protected_ordinal),
        parity_state: Some(parity_state),
        plaintext_digest: object_digest,
        stored_digest: object_digest,
    };
    let mut tape_file_bundle = object_close.committed_bundle()?;
    for entry in &mut tape_file_bundle.entries {
        if entry.kind == TapeFileKind::Object
            && entry.tape_file_number == object_close.tape_file_number
        {
            entry.object_id = Some(prepared.options.object_id.clone());
        }
    }
    let catalog = StreamingCatalogProjection {
        object,
        files,
        object_copy,
        tape_file_bundle,
    };
    let audit_events = vec![StreamingAuditEvent {
        kind: "canonical_plaintext_object_committed",
        object_id: prepared.options.object_id.clone(),
        summary: format!(
            "committed canonical plaintext REM object {} to tape file {} ({} payload files, {} object blocks)",
            prepared.options.object_id,
            object_close.tape_file_number,
            prepared.files.len(),
            object_close.data_block_count
        ),
    }];
    Ok(StreamingObjectWriteReport {
        layout,
        object_close,
        catalog,
        audit_events,
    })
}

pub(crate) fn write_encrypted_object_to_parity(
    parity: &mut ParitySink<'_>,
    tape_uuid: TapeUuid,
    prepared: &PreparedPoolObject,
    encrypted: &PreparedEncryptedPoolObject,
    capacity: TerminalTripleObjectReservation,
) -> Result<StreamingObjectWriteReport, PoolWriteError> {
    let opened = parity.begin_object_with_terminal_triple_reservation(capacity)?;
    let blocks_written = write_fixed_blocks(
        parity,
        prepared.options.chunk_size,
        encrypted.sealed.as_slice(),
    )?;
    if blocks_written != encrypted.envelope.stored_size_blocks {
        return Err(PoolWriteError::InvalidInput(
            "encrypted REM-OBJECT write count differs from envelope".to_string(),
        ));
    }
    let object_close = parity.finish_object()?;
    if opened.0 != object_close.tape_file_number {
        return Err(PoolWriteError::InvalidInput(
            "parity encrypted object tape-file number changed during write".to_string(),
        ));
    }
    encrypted_write_report(
        tape_uuid,
        prepared,
        encrypted,
        object_close,
        "streaming_encrypted_object_committed",
        "committed encrypted object",
        None,
    )
}

pub(crate) fn no_parity_encrypted_write_report(
    tape_uuid: TapeUuid,
    prepared: &PreparedPoolObject,
    encrypted: &PreparedEncryptedPoolObject,
    filemark_outcome: remanence_library::WriteFilemarksOutcome,
    append: NoParityAppendContext,
) -> Result<StreamingObjectWriteReport, PoolWriteError> {
    let total_committed_ordinals =
        append.object_total_committed_ordinals(encrypted.envelope.stored_size_blocks)?;
    let object_close = ObjectWriteSummary {
        tape_file_number: append.tape_file_number,
        first_parity_data_ordinal: 0,
        projected_size_blocks: encrypted.envelope.stored_size_blocks,
        data_block_count: encrypted.envelope.stored_size_blocks,
        filemark_outcome,
        sidecars_emitted: Vec::new(),
        highest_protected_ordinal: 0,
        physical_start_lba: Some(append.object_start_lba()?),
    };
    encrypted_write_report(
        tape_uuid,
        prepared,
        encrypted,
        object_close,
        "streaming_encrypted_object_committed_no_parity",
        "committed no-parity encrypted object",
        Some(UnprotectedObjectBundleContext {
            fresh_tape: append.fresh_tape,
            total_committed_ordinals,
        }),
    )
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct UnprotectedObjectBundleContext {
    pub(crate) fresh_tape: bool,
    pub(crate) total_committed_ordinals: u64,
}

pub(crate) fn encrypted_write_report(
    tape_uuid: TapeUuid,
    prepared: &PreparedPoolObject,
    encrypted: &PreparedEncryptedPoolObject,
    object_close: ObjectWriteSummary,
    audit_kind: &'static str,
    audit_prefix: &'static str,
    unprotected_context: Option<UnprotectedObjectBundleContext>,
) -> Result<StreamingObjectWriteReport, PoolWriteError> {
    if prepared.files.len() != encrypted.plaintext_layout.files.len() {
        return Err(PoolWriteError::InvalidInput(
            "prepared file count does not match encrypted plaintext layout".to_string(),
        ));
    }
    if object_close.data_block_count != encrypted.envelope.stored_size_blocks {
        return Err(PoolWriteError::InvalidInput(
            "encrypted object close block count differs from envelope".to_string(),
        ));
    }
    let logical_size_bytes =
        encrypted
            .plaintext_layout
            .files
            .iter()
            .try_fold(0u64, |acc, file| {
                acc.checked_add(file.size_bytes).ok_or_else(|| {
                    PoolWriteError::InvalidInput("logical size overflow".to_string())
                })
            })?;
    let files = encrypted
        .plaintext_layout
        .files
        .iter()
        .zip(prepared.files.iter())
        .map(|(file, prepared_file)| {
            no_parity_file_catalog_projection(&prepared.options.object_id, file, prepared_file)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let object = ObjectCatalogProjection {
        object_id: prepared.options.object_id.clone(),
        caller_object_id: prepared.options.caller_object_id.clone(),
        body_format: FORMAT_ID.to_string(),
        logical_size_bytes,
        manifest_sha256: encrypted.plaintext_layout.manifest_sha256,
    };
    let parity_state = if object_close.highest_protected_ordinal > 0 {
        Some(remanence_parity::ObjectParityState::from_ordinals(
            object_close.first_parity_data_ordinal,
            object_close.data_block_count,
            object_close.highest_protected_ordinal,
        )?)
    } else {
        None
    };
    let object_copy = ObjectCopyProjection {
        object_id: prepared.options.object_id.clone(),
        tape_uuid,
        tape_file_number: object_close.tape_file_number,
        first_parity_data_ordinal: (object_close.highest_protected_ordinal > 0)
            .then_some(object_close.first_parity_data_ordinal),
        data_block_count: object_close.data_block_count,
        protected_until_ordinal: (object_close.highest_protected_ordinal > 0)
            .then_some(object_close.highest_protected_ordinal),
        parity_state,
        plaintext_digest: encrypted.envelope.plaintext.digest,
        stored_digest: encrypted.envelope.stored_digest,
    };
    let tape_file_bundle = if object_close.highest_protected_ordinal > 0 {
        let mut bundle = object_close.committed_bundle()?;
        for entry in &mut bundle.entries {
            if entry.kind == TapeFileKind::Object
                && entry.tape_file_number == object_close.tape_file_number
            {
                entry.object_id = Some(prepared.options.object_id.clone());
            }
        }
        bundle
    } else {
        let unprotected_context = unprotected_context.ok_or_else(|| {
            PoolWriteError::InvalidInput(
                "unprotected encrypted object is missing commit context".to_string(),
            )
        })?;
        let mut entries = Vec::with_capacity(if unprotected_context.fresh_tape { 2 } else { 1 });
        if unprotected_context.fresh_tape {
            entries.push(TapeFileEntry {
                tape_file_number: 0,
                kind: TapeFileKind::Bootstrap,
                block_count: 1,
                // A fresh tape's bootstrap prefix starts at BOT by definition.
                physical_start_hint: Some(0),
                object_id: None,
                first_parity_data_ordinal: None,
                epoch_id: None,
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                canonical_metadata_hash: None,
                object_recovery_row: None,
            });
        }
        entries.push(TapeFileEntry {
            tape_file_number: object_close.tape_file_number,
            kind: TapeFileKind::Object,
            block_count: encrypted.envelope.stored_size_blocks,
            physical_start_hint: object_close.physical_start_lba,
            object_id: Some(prepared.options.object_id.clone()),
            first_parity_data_ordinal: None,
            epoch_id: None,
            protected_ordinal_start: None,
            protected_ordinal_end_exclusive: None,
            canonical_metadata_hash: None,
            object_recovery_row: None,
        });
        CommittedBundle {
            kind: CommittedBundleKind::Object,
            entries,
            highest_protected_ordinal: 0,
            total_committed_ordinals: unprotected_context.total_committed_ordinals,
        }
    };
    let catalog = StreamingCatalogProjection {
        object,
        files,
        object_copy,
        tape_file_bundle,
    };
    let audit_events = vec![StreamingAuditEvent {
        kind: audit_kind,
        object_id: prepared.options.object_id.clone(),
        summary: format!(
            "{audit_prefix} {} to tape file {} ({} payload files, {} stored blocks)",
            prepared.options.object_id,
            object_close.tape_file_number,
            prepared.files.len(),
            object_close.data_block_count
        ),
    }];
    Ok(StreamingObjectWriteReport {
        layout: encrypted.plaintext_layout.clone(),
        object_close,
        catalog,
        audit_events,
    })
}
