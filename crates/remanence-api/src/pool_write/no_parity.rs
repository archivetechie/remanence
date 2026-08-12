//! No-parity writes, projection, replay, and tape-I/O fencing.

use std::sync::Arc;
use std::time::{Duration, Instant};

use remanence_format::{
    write_rem_tar_object_from_readers, RemTarFileLayout, RemTarFileStream, FORMAT_ID,
};
use remanence_library::BlockSink;
use remanence_parity::TapeFileKind;
use remanence_state::{
    CatalogIndex, NativeObjectCopyProjectionInput, NativeObjectCopyRecord,
    NativeObjectFileProjectionInput, NativeObjectProjectionInput, NativeObjectRecord,
    TapePoolConfig, OBJECT_COPY_REPRESENTATION_ENCRYPTED, OBJECT_COPY_REPRESENTATION_PLAINTEXT,
};
use remanence_stream::{
    normalize_archive_path, FileCatalogProjection, PreparedFile, StreamingError,
    StreamingObjectWriteReport,
};
use uuid::Uuid;

use super::capacity::{first_payload_body_lba, uuid_text};
use super::model::{
    AppendCommitDiagnostics, BatchedAppendPosition, BatchedNoParityAppendContext,
    PoolWriteDurability, PoolWriteError, PoolWriteObjectCopyRecord, PoolWriteObjectRecord,
    PoolWriteRepresentation, PoolWriteResult, SelectedTape, WriteObjectInputKind,
    WriteObjectToPoolRequest,
};
use super::overlap::with_overlap_sink;
use super::prepare::{
    log_transfer_diagnostics, no_parity_encrypted_write_report, no_parity_write_report,
    open_prepared_readers, position_no_parity_append_at_checkpoint,
    prove_no_parity_append_boundary, validate_input_kind_guards, write_canonical_plaintext_blocks,
    write_fixed_blocks, write_no_parity_bootstrap, write_object_delimiter, CopyRepresentation,
    PreparedPoolObject, PreparedPoolWrite, PreparedStoredObject, TransferDiagnosticOutcome,
};
use super::staging::{
    run_faulted_counted_fenced_staged_transfer, CountingBlockSink, ObjectDigestBlockSink,
};
use crate::bytes_to_hex;

#[cfg(test)]
use super::capacity::{no_parity_append_context, seal_selected_tape_if_needed};
#[cfg(test)]
use super::prepare::{log_commit_diagnostics, position_no_parity_append};
#[cfg(test)]
use remanence_parity::ParityScheme;
#[cfg(test)]
use remanence_state::TapeJournalIndexInput;

pub(super) fn write_no_parity_object_to_selected_tape<S: BlockSink + ?Sized>(
    state: &mut CatalogIndex,
    sink: &mut CountingBlockSink<'_, S>,
    _pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    prepared_write: PreparedPoolWrite,
    durability: PoolWriteDurability,
) -> Result<PoolWriteResult, PoolWriteError> {
    let PreparedPoolWrite { prepared, stored } = prepared_write;
    let tape_uuid = selected.tape_uuid;
    let append = match &durability {
        #[cfg(test)]
        PoolWriteDurability::PerObject => no_parity_append_context(state, &selected)?,
        PoolWriteDurability::Batched(context) => context.append,
    };
    let expected_initial_lba = append.expected_append_lba()?;
    let overlap_control = prepared.overlap_control();
    let object_fault = crate::object_fault::ObjectFaultPlan::from_env_for_object(
        selected.tape_uuid,
        &request.caller_object_id,
    )
    .map_err(PoolWriteError::InvalidInput)?;
    if object_fault.is_some() && append.fresh_tape {
        return Err(PoolWriteError::InvalidInput(
            "WOR object fault requires a prior committed checkpoint so its record count is object-relative"
                .to_string(),
        ));
    }
    let transfer_started = Instant::now();
    let write_report: Result<StreamingObjectWriteReport, PoolWriteError> =
        run_faulted_counted_fenced_staged_transfer(
            state,
            &selected,
            sink,
            selected.block_size as usize,
            overlap_control.as_ref().map(Arc::clone),
            object_fault.as_ref(),
            |staged| {
                with_overlap_sink(staged, overlap_control, expected_initial_lba, |gated| {
                    match &durability {
                        #[cfg(test)]
                        PoolWriteDurability::PerObject if append.fresh_tape => {
                            write_no_parity_bootstrap(
                                gated,
                                tape_uuid,
                                selected.block_size,
                                &prepared.write_timestamp,
                            )?
                        }
                        PoolWriteDurability::Batched(BatchedNoParityAppendContext {
                            position: BatchedAppendPosition::FreshTape,
                            ..
                        }) => write_no_parity_bootstrap(
                            gated,
                            tape_uuid,
                            selected.block_size,
                            &prepared.write_timestamp,
                        )?,
                        #[cfg(test)]
                        PoolWriteDurability::PerObject => {
                            position_no_parity_append(gated)?;
                        }
                        PoolWriteDurability::Batched(BatchedNoParityAppendContext {
                            position: BatchedAppendPosition::JournalEod(lba),
                            ..
                        }) => position_no_parity_append_at_checkpoint(gated, *lba)?,
                        PoolWriteDurability::Batched(BatchedNoParityAppendContext {
                            position: BatchedAppendPosition::CurrentBoundary(lba),
                            ..
                        }) => prove_no_parity_append_boundary(gated, *lba)?,
                    }
                    let report = match &stored {
                        PreparedStoredObject::Plaintext => {
                            let mut readers = open_prepared_readers(&prepared)?;
                            let mut streams = Vec::with_capacity(prepared.files.len());
                            for (file, reader) in prepared.files.iter().zip(readers.iter_mut()) {
                                streams.push(RemTarFileStream::new(
                                    file.spec.clone(),
                                    reader.as_mut(),
                                ));
                            }
                            let mut object_sink = ObjectDigestBlockSink::new(gated);
                            let layout = write_rem_tar_object_from_readers(
                                &mut object_sink,
                                &prepared.options,
                                &mut streams,
                            )
                            .map_err(StreamingError::from)?;
                            let object_digest = object_sink.finish_digest();
                            let filemark_outcome = write_object_delimiter(
                                gated,
                                &durability,
                                append,
                                layout.projected_size_blocks,
                            )?;
                            no_parity_write_report(
                                tape_uuid,
                                &prepared,
                                layout,
                                object_digest,
                                filemark_outcome,
                                append,
                            )
                        }
                        PreparedStoredObject::CanonicalPlaintext => {
                            let object_digest = write_canonical_plaintext_blocks(gated, &prepared)?;
                            let filemark_outcome = write_object_delimiter(
                                gated,
                                &durability,
                                append,
                                prepared.plan.layout.projected_size_blocks,
                            )?;
                            no_parity_write_report(
                                tape_uuid,
                                &prepared,
                                prepared.plan.layout.clone(),
                                object_digest,
                                filemark_outcome,
                                append,
                            )
                        }
                        PreparedStoredObject::Encrypted(encrypted) => {
                            write_fixed_blocks(
                                gated,
                                prepared.options.chunk_size,
                                &encrypted.sealed,
                            )?;
                            let filemark_outcome = write_object_delimiter(
                                gated,
                                &durability,
                                append,
                                encrypted.envelope.stored_size_blocks,
                            )?;
                            no_parity_encrypted_write_report(
                                tape_uuid,
                                &prepared,
                                encrypted,
                                filemark_outcome,
                                append,
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
                matches!(&durability, PoolWriteDurability::Batched(_)),
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
                matches!(&durability, PoolWriteDurability::Batched(_)),
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
    let (write_report, _transfer_stats) = write_report;

    match durability {
        PoolWriteDurability::Batched(_) => {
            let checkpoint_projection = checkpoint_projection_for_no_parity_write(
                &selected,
                &prepared,
                &write_report,
                stored.copy_representation(),
            )?;
            pool_write_result(
                request,
                selected,
                prepared,
                stored.copy_representation(),
                write_report,
                AppendCommitDiagnostics {
                    filemark_write_drain: Duration::ZERO,
                    catalog_journal_fsync: Duration::ZERO,
                },
                false,
                Some(checkpoint_projection),
            )
        }
        #[cfg(test)]
        PoolWriteDurability::PerObject => {
            let commit_started = Instant::now();
            let commit_result = commit_pool_write(
                state,
                &selected,
                &prepared,
                &write_report,
                CommitPoolWriteProjection {
                    first_parity_data_ordinal: None,
                    protected_until_ordinal: None,
                    scheme: None,
                    copy_representation: stored.copy_representation(),
                },
                _pool_cfg,
                _transfer_stats.early_warning,
            );
            let commit_elapsed = commit_started.elapsed();
            let sealed_after_write = match commit_result {
                Ok(sealed_after_write) => {
                    log_commit_diagnostics(
                        &request,
                        &selected,
                        &prepared,
                        commit_elapsed,
                        "ok",
                        None,
                    );
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
                    filemark_write_drain: _transfer_stats.filemark_write_drain,
                    catalog_journal_fsync: commit_elapsed,
                },
                sealed_after_write,
                None,
            )
        }
    }
}

pub(super) fn record_tape_io_fence_for_transfer_error(
    state: &mut CatalogIndex,
    selected: &SelectedTape,
    reason: &str,
    error: &str,
) -> Result<(), PoolWriteError> {
    let barcode = state
        .get_tape(&selected.tape_uuid)?
        .and_then(|tape| tape.voltag);
    let evidence = format!(
        "{{\"pool_id\":\"{}\",\"tape_uuid\":\"{}\",\"error\":\"{}\"}}",
        json_escape(selected.pool_id.as_str()),
        uuid_text(selected.tape_uuid),
        json_escape(error),
    );
    state.record_tape_io_fence(remanence_state::TapeIoFenceInput {
        tape_uuid: selected.tape_uuid,
        barcode,
        reason: reason.to_string(),
        evidence_json: Some(evidence),
    })?;
    Ok(())
}

pub(super) fn fence_after_terminal_motion(
    state: &mut CatalogIndex,
    selected: &SelectedTape,
    reason: &str,
    error: PoolWriteError,
) -> PoolWriteError {
    let detail = error.to_string();
    match record_tape_io_fence_for_transfer_error(state, selected, reason, detail.as_str()) {
        Ok(()) => error,
        Err(fence_error) => PoolWriteError::InvalidInput(format!(
            "{detail}; failed to persist required terminal tape-I/O fence: {fence_error}"
        )),
    }
}

pub(super) fn tape_io_fence_reason_for_transfer_error(error: &str) -> &'static str {
    if error.contains("reset UNIT ATTENTION") {
        "reset_unit_attention"
    } else if error.contains("partial fixed batch uncommittable") {
        "partial_batch"
    } else if error.contains("position drift") {
        "position_drift"
    } else {
        "transfer_error"
    }
}

pub(super) fn json_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

#[cfg(test)]
pub(super) struct CommitPoolWriteProjection {
    pub(super) first_parity_data_ordinal: Option<u64>,
    pub(super) protected_until_ordinal: Option<u64>,
    pub(super) scheme: Option<ParityScheme>,
    pub(super) copy_representation: CopyRepresentation,
}

#[cfg(test)]
pub(super) fn commit_pool_write(
    state: &mut CatalogIndex,
    selected: &SelectedTape,
    prepared: &PreparedPoolObject,
    write_report: &StreamingObjectWriteReport,
    projection: CommitPoolWriteProjection,
    pool_cfg: &TapePoolConfig,
    hardware_early_warning: bool,
) -> Result<bool, PoolWriteError> {
    let first_body_lba = first_payload_body_lba(write_report);
    let metadata_hash =
        if projection.copy_representation.representation == OBJECT_COPY_REPRESENTATION_PLAINTEXT {
            Some(write_report.catalog.object.manifest_sha256.to_vec())
        } else {
            None
        };
    let file_projections = write_report
        .catalog
        .files
        .iter()
        .map(native_object_file_projection)
        .collect::<Vec<_>>();
    let object_projection = NativeObjectProjectionInput {
        object_id: write_report.catalog.object.object_id.clone(),
        caller_object_id: Some(write_report.catalog.object.caller_object_id.clone()),
        body_format: write_report.catalog.object.body_format.clone(),
        logical_size_bytes: Some(write_report.catalog.object.logical_size_bytes),
        content_hash: Some(prepared.content_sha256.to_vec()),
        metadata_hash,
        created_at_utc: Some(prepared.write_timestamp.clone()),
    };
    let copy_projection = NativeObjectCopyProjectionInput {
        object_id: write_report.catalog.object_copy.object_id.clone(),
        tape_uuid: selected.tape_uuid,
        tape_file_number: write_report.catalog.object_copy.tape_file_number,
        first_body_lba,
        first_parity_data_ordinal: projection.first_parity_data_ordinal,
        protected_until_ordinal: projection.protected_until_ordinal,
        status: "committed".to_string(),
        representation: projection.copy_representation.representation.to_string(),
        recipient_epoch_ids: projection.copy_representation.recipient_epoch_ids.clone(),
        metadata_frame_len: projection.copy_representation.metadata_frame_len,
        plaintext_digest: Some(write_report.catalog.object_copy.plaintext_digest.to_vec()),
        stored_digest: Some(write_report.catalog.object_copy.stored_digest.to_vec()),
    };
    let tape_input = TapeJournalIndexInput {
        tape_uuid: selected.tape_uuid,
        block_size: selected.block_size,
        scheme: projection.scheme,
        journal_offset_bytes: 0,
    };
    if tape_input.scheme.is_none() {
        state.project_native_object_append_commit(
            object_projection,
            &file_projections,
            &[copy_projection],
            tape_input,
            &write_report.catalog.tape_file_bundle,
        )?;
    } else {
        state.project_native_object_and_committed_tape_file_bundle(
            object_projection,
            &file_projections,
            &[copy_projection],
            tape_input,
            &write_report.catalog.tape_file_bundle,
        )?;
    }
    seal_selected_tape_if_needed(state, selected, pool_cfg, hardware_early_warning)
}

pub(super) fn checkpoint_projection_for_no_parity_write(
    selected: &SelectedTape,
    prepared: &PreparedPoolObject,
    write_report: &StreamingObjectWriteReport,
    copy_representation: CopyRepresentation,
) -> Result<remanence_state::CheckpointObjectProjection, PoolWriteError> {
    let file_projections = write_report
        .catalog
        .files
        .iter()
        .map(native_object_file_projection)
        .collect();
    let representation = match copy_representation.representation {
        OBJECT_COPY_REPRESENTATION_PLAINTEXT => {
            let manifest_first_chunk_lba = prepared
                .plan
                .layout
                .manifest
                .first_chunk_lba
                .ok_or_else(|| {
                    PoolWriteError::InvalidInput(
                        "prepared plaintext manifest has no body LBA".to_string(),
                    )
                })?;
            remanence_state::CheckpointObjectRecoveryRepresentation::Plaintext {
                manifest_first_chunk_lba: manifest_first_chunk_lba.0,
                manifest_size_bytes: prepared.plan.layout.manifest.size_bytes,
                manifest_chunk_count: prepared.plan.layout.manifest.chunk_count,
                manifest_sha256: prepared.plan.layout.manifest_sha256,
            }
        }
        OBJECT_COPY_REPRESENTATION_ENCRYPTED => {
            remanence_state::CheckpointObjectRecoveryRepresentation::Encrypted {
                recipient_epoch_ids: copy_representation
                    .recovery_recipient_epoch_ids
                    .clone()
                    .ok_or_else(|| {
                        PoolWriteError::InvalidInput(
                            "prepared encrypted copy has no recipient epochs".to_string(),
                        )
                    })?,
                metadata_frame_len: copy_representation.metadata_frame_len.ok_or_else(|| {
                    PoolWriteError::InvalidInput(
                        "prepared encrypted copy has no metadata frame length".to_string(),
                    )
                })?,
                key_frame_len: copy_representation.key_frame_len.ok_or_else(|| {
                    PoolWriteError::InvalidInput(
                        "prepared encrypted copy has no key frame length".to_string(),
                    )
                })?,
            }
        }
        other => {
            return Err(PoolWriteError::InvalidInput(format!(
                "unsupported prepared copy representation {other:?}"
            )));
        }
    };
    Ok(remanence_state::CheckpointObjectProjection {
        object: NativeObjectProjectionInput {
            object_id: write_report.catalog.object.object_id.clone(),
            caller_object_id: Some(write_report.catalog.object.caller_object_id.clone()),
            body_format: write_report.catalog.object.body_format.clone(),
            logical_size_bytes: Some(write_report.catalog.object.logical_size_bytes),
            content_hash: Some(prepared.content_sha256.to_vec()),
            metadata_hash: (copy_representation.representation
                == OBJECT_COPY_REPRESENTATION_PLAINTEXT)
                .then(|| write_report.catalog.object.manifest_sha256.to_vec()),
            created_at_utc: Some(prepared.write_timestamp.clone()),
        },
        files: file_projections,
        copy: NativeObjectCopyProjectionInput {
            object_id: write_report.catalog.object_copy.object_id.clone(),
            tape_uuid: selected.tape_uuid,
            tape_file_number: write_report.catalog.object_copy.tape_file_number,
            first_body_lba: first_payload_body_lba(write_report),
            first_parity_data_ordinal: None,
            protected_until_ordinal: None,
            status: "committed".to_string(),
            representation: copy_representation.representation.to_string(),
            recipient_epoch_ids: copy_representation.recipient_epoch_ids,
            metadata_frame_len: copy_representation.metadata_frame_len,
            plaintext_digest: Some(write_report.catalog.object_copy.plaintext_digest.to_vec()),
            stored_digest: Some(write_report.catalog.object_copy.stored_digest.to_vec()),
        },
        block_size: selected.block_size,
        block_count: write_report.catalog.object_copy.data_block_count,
        fresh_tape: write_report
            .catalog
            .tape_file_bundle
            .entries
            .first()
            .is_some_and(|entry| entry.kind == TapeFileKind::Bootstrap),
        total_committed_ordinals: write_report
            .catalog
            .tape_file_bundle
            .total_committed_ordinals,
        object_recovery_row: remanence_state::CheckpointObjectRecoveryRow {
            tape_file_number: write_report.catalog.object_copy.tape_file_number,
            stored_block_count: write_report.catalog.object_copy.data_block_count,
            object_id: prepared.options.object_id.as_bytes().to_vec(),
            representation,
        },
    })
}

pub(super) fn native_object_file_projection(
    file: &FileCatalogProjection,
) -> NativeObjectFileProjectionInput {
    NativeObjectFileProjectionInput {
        object_id: file.object_id.clone(),
        file_id: file.file_id.clone(),
        path: file.path.clone(),
        size_bytes: file.size_bytes,
        file_sha256: file.file_sha256.to_vec(),
        first_chunk_lba: file.first_chunk_lba.map(|lba| lba.0),
        chunk_count: file.chunk_count,
        mtime: file.mtime.clone(),
        executable: file.executable,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn pool_write_result(
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    prepared: PreparedPoolObject,
    copy_representation: CopyRepresentation,
    write_report: StreamingObjectWriteReport,
    append_commit_diagnostics: AppendCommitDiagnostics,
    sealed_after_write: bool,
    checkpoint_projection: Option<remanence_state::CheckpointObjectProjection>,
) -> Result<PoolWriteResult, PoolWriteError> {
    let input_kind = request.input_kind;
    let position_lba = write_report
        .object_close
        .filemark_outcome
        .position_after
        .lba;
    let post_write_used_bytes = checked_physical_used_bytes(position_lba, selected.block_size)?;
    let hardware_early_warning = write_report.object_close.filemark_outcome.early_warning
        || write_report
            .object_close
            .sidecars_emitted
            .iter()
            .any(|sidecar| sidecar.filemark_outcome.early_warning);
    let first_body_lba = first_payload_body_lba(&write_report);
    let object = PoolWriteObjectRecord {
        object_id: *prepared.object_uuid.as_bytes(),
        caller_object_id: request.caller_object_id,
        content_sha256: prepared.content_sha256,
        logical_size_bytes: write_report.catalog.object.logical_size_bytes,
        body_format: FORMAT_ID.to_string(),
        created_at_utc: prepared.write_timestamp,
        copies: vec![PoolWriteObjectCopyRecord {
            tape_uuid: selected.tape_uuid,
            tape_file_number: write_report.catalog.object_copy.tape_file_number,
            first_body_lba,
            pool_id: selected.pool_id,
            representation: copy_representation.representation.to_string(),
            recipient_epoch_ids: copy_representation.recipient_epoch_ids,
            metadata_frame_len: copy_representation.metadata_frame_len,
            plaintext_digest: Some(write_report.catalog.object_copy.plaintext_digest),
            stored_digest: Some(write_report.catalog.object_copy.stored_digest),
        }],
    };

    Ok(PoolWriteResult {
        object,
        write_report: Some(write_report),
        append_commit_diagnostics,
        sealed_after_write,
        checkpoint_projection,
        post_write_used_bytes,
        hardware_early_warning,
        input_kind,
    })
}

pub(super) fn checked_physical_used_bytes(
    position_lba: u64,
    block_size: u32,
) -> Result<u64, PoolWriteError> {
    position_lba.checked_mul(u64::from(block_size)).ok_or(
        PoolWriteError::PhysicalUsedBytesOverflow {
            position_lba,
            block_size,
        },
    )
}

pub(crate) fn maybe_replay_pool_write(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    request: &WriteObjectToPoolRequest,
) -> Result<Option<PoolWriteResult>, PoolWriteError> {
    validate_input_kind_guards(request)?;
    if request.caller_object_id.trim().is_empty() {
        return Ok(None);
    }
    let Some(mut existing) = state.get_native_object_by_pool_and_caller_object_id(
        pool_cfg.id.as_str(),
        request.caller_object_id.as_str(),
    )?
    else {
        if let Some(expected_object_id) = request.expected_object_id {
            let object_id = Uuid::from_bytes(expected_object_id).to_string();
            if state.get_native_object(&object_id)?.is_some() {
                return Err(PoolWriteError::InvalidInput(format!(
                    "canonical plaintext REM object id {object_id} already exists outside the exact pool/caller replay key; attaching another copy is not supported by this append surface"
                )));
            }
        }
        return Ok(None);
    };
    if let Some(expected_object_id) = request.expected_object_id {
        let existing_object_id = Uuid::parse_str(&existing.object_id).map_err(|error| {
            replay_object_invalid(
                &existing.object_id,
                format!("object_id is not a UUID: {error}"),
            )
        })?;
        if existing_object_id.as_bytes() != &expected_object_id {
            return Err(PoolWriteError::InvalidInput(format!(
                "canonical plaintext REM object replay identity mismatch: committed={}, expected={}",
                existing_object_id,
                Uuid::from_bytes(expected_object_id)
            )));
        }
    }
    let existing_hash = native_object_content_sha256(&existing)?;
    let existing_files = state.list_native_object_files(existing.object_id.as_str())?;
    let existing_input_kind =
        committed_native_object_input_kind(&existing, existing_hash, &existing_files)?;
    if existing_input_kind != request.input_kind {
        return Err(PoolWriteError::CallerObjectIdInputKindConflict {
            pool_id: pool_cfg.id.clone(),
            caller_object_id: request.caller_object_id.clone(),
            existing_input_kind,
            requested_input_kind: request.input_kind,
        });
    }
    if request.input_kind == WriteObjectInputKind::LogicalFile {
        let requested_archive_path = normalize_archive_path(&request.archive_path)?;
        let existing_archive_path = existing_files
            .first()
            .expect("logical-file replay classification requires one member")
            .path
            .as_str();
        if existing_archive_path != requested_archive_path.as_str() {
            return Err(PoolWriteError::CallerObjectIdArchivePathConflict {
                pool_id: pool_cfg.id.clone(),
                caller_object_id: request.caller_object_id.clone(),
                existing_archive_path: existing_archive_path.to_string(),
                requested_archive_path,
            });
        }
    }
    let (requested_representation, requested_recipient_epoch_ids) =
        requested_copy_identity(&request.representation);
    let existing_representations = existing
        .copies
        .iter()
        .filter(|copy| {
            copy.pool_id.as_deref() == Some(pool_cfg.id.as_str()) && copy.status == "committed"
        })
        .map(committed_copy_identity_summary)
        .collect::<Vec<_>>();
    existing.copies.retain(|copy| {
        copy.pool_id.as_deref() == Some(pool_cfg.id.as_str())
            && copy.status == "committed"
            && copy.representation == requested_representation
            && copy.recipient_epoch_ids == requested_recipient_epoch_ids
    });
    if existing.copies.is_empty() {
        return Err(PoolWriteError::CallerObjectIdRepresentationConflict {
            pool_id: pool_cfg.id.clone(),
            caller_object_id: request.caller_object_id.clone(),
            existing_representations,
            requested_representation,
            requested_recipient_epoch_ids,
        });
    }
    let _ = request.source.size_bytes()?;
    let requested_hash = request.source.content_sha256()?;
    if let Some(expected) = request.expected_content_sha256 {
        if requested_hash != expected {
            return Err(PoolWriteError::ContentHashMismatch {
                expected: bytes_to_hex(&expected),
                actual: bytes_to_hex(&requested_hash),
            });
        }
    }
    if existing_hash != requested_hash {
        return Err(PoolWriteError::CallerObjectIdConflict {
            pool_id: pool_cfg.id.clone(),
            caller_object_id: request.caller_object_id.clone(),
            existing_content_sha256: bytes_to_hex(&existing_hash),
            requested_content_sha256: bytes_to_hex(&requested_hash),
        });
    }
    Ok(Some(PoolWriteResult {
        object: pool_write_object_record_from_native(existing, pool_cfg.id.as_str())?,
        write_report: None,
        append_commit_diagnostics: AppendCommitDiagnostics::default(),
        sealed_after_write: false,
        checkpoint_projection: None,
        post_write_used_bytes: 0,
        hardware_early_warning: false,
        input_kind: request.input_kind,
    }))
}

pub(super) fn committed_native_object_input_kind(
    object: &NativeObjectRecord,
    content_sha256: [u8; 32],
    files: &[remanence_state::NativeObjectFileRecord],
) -> Result<WriteObjectInputKind, PoolWriteError> {
    let logical_size_bytes = object
        .logical_size_bytes
        .ok_or_else(|| replay_object_invalid(&object.object_id, "logical_size_bytes is missing"))?;
    if files.is_empty() {
        return Err(replay_object_invalid(
            &object.object_id,
            "member projection is missing, so the committed input kind cannot be proved",
        ));
    }
    let is_logical_file = files.len() == 1
        && files[0].size_bytes == logical_size_bytes
        && files[0].file_digest_algorithm == remanence_state::DIGEST_ALGORITHM_SHA256
        && files[0].file_sha256.as_slice() == content_sha256;
    Ok(if is_logical_file {
        WriteObjectInputKind::LogicalFile
    } else {
        WriteObjectInputKind::CanonicalPlaintextRemObject
    })
}

pub(super) fn requested_copy_identity(
    representation: &PoolWriteRepresentation,
) -> (&'static str, Option<Vec<String>>) {
    match representation {
        PoolWriteRepresentation::Plaintext => (OBJECT_COPY_REPRESENTATION_PLAINTEXT, None),
        PoolWriteRepresentation::Encrypted { recipients } => (
            OBJECT_COPY_REPRESENTATION_ENCRYPTED,
            Some(
                recipients
                    .iter()
                    .map(|recipient| bytes_to_hex(&recipient.recipient_epoch_id))
                    .collect(),
            ),
        ),
    }
}

pub(super) fn committed_copy_identity_summary(copy: &NativeObjectCopyRecord) -> String {
    match copy.recipient_epoch_ids.as_deref() {
        Some(recipient_epoch_ids) => {
            format!("{}:{recipient_epoch_ids:?}", copy.representation)
        }
        None => copy.representation.clone(),
    }
}

pub(super) fn pool_write_object_record_from_native(
    object: NativeObjectRecord,
    pool_id: &str,
) -> Result<PoolWriteObjectRecord, PoolWriteError> {
    let object_uuid = Uuid::parse_str(object.object_id.as_str()).map_err(|err| {
        replay_object_invalid(&object.object_id, format!("object_id is not a UUID: {err}"))
    })?;
    let content_sha256 = native_object_content_sha256(&object)?;
    let logical_size_bytes = object
        .logical_size_bytes
        .ok_or_else(|| replay_object_invalid(&object.object_id, "logical_size_bytes is missing"))?;
    let copies = object
        .copies
        .iter()
        .filter(|copy| copy.pool_id.as_deref() == Some(pool_id) && copy.status == "committed")
        .map(|copy| pool_write_copy_record_from_native(copy, pool_id))
        .collect::<Result<Vec<_>, _>>()?;
    if copies.is_empty() {
        return Err(replay_object_invalid(
            &object.object_id,
            format!("no committed copy in pool {pool_id}"),
        ));
    }
    Ok(PoolWriteObjectRecord {
        object_id: *object_uuid.as_bytes(),
        caller_object_id: object.caller_object_id.unwrap_or_default(),
        content_sha256,
        logical_size_bytes,
        body_format: object.body_format,
        created_at_utc: object.created_at_utc,
        copies,
    })
}

pub(super) fn pool_write_copy_record_from_native(
    copy: &NativeObjectCopyRecord,
    pool_id: &str,
) -> Result<PoolWriteObjectCopyRecord, PoolWriteError> {
    let tape_uuid =
        copy.tape_uuid.as_slice().try_into().map_err(|_| {
            replay_object_invalid(&copy.object_id, "copy tape_uuid is not 16 bytes")
        })?;
    Ok(PoolWriteObjectCopyRecord {
        tape_uuid,
        tape_file_number: copy.tape_file_number,
        first_body_lba: copy.first_body_lba,
        pool_id: pool_id.to_string(),
        representation: copy.representation.clone(),
        recipient_epoch_ids: copy.recipient_epoch_ids.clone(),
        metadata_frame_len: copy.metadata_frame_len,
        plaintext_digest: optional_native_copy_digest(
            copy.plaintext_digest.as_deref(),
            &copy.object_id,
            "plaintext_digest",
        )?,
        stored_digest: optional_native_copy_digest(
            copy.stored_digest.as_deref(),
            &copy.object_id,
            "stored_digest",
        )?,
    })
}

pub(super) fn optional_native_copy_digest(
    digest: Option<&[u8]>,
    object_id: &str,
    field: &str,
) -> Result<Option<[u8; 32]>, PoolWriteError> {
    digest
        .map(|digest| {
            digest
                .try_into()
                .map_err(|_| replay_object_invalid(object_id, format!("{field} is not 32 bytes")))
        })
        .transpose()
}

pub(super) fn native_object_content_sha256(
    object: &NativeObjectRecord,
) -> Result<[u8; 32], PoolWriteError> {
    let Some(content_hash) = object.content_hash.as_deref() else {
        return Err(replay_object_invalid(
            &object.object_id,
            "content_hash is missing",
        ));
    };
    content_hash
        .try_into()
        .map_err(|_| replay_object_invalid(&object.object_id, "content_hash is not 32 bytes"))
}

pub(super) fn replay_object_invalid(object_id: &str, reason: impl Into<String>) -> PoolWriteError {
    PoolWriteError::ReplayObjectInvalid {
        object_id: object_id.to_string(),
        reason: reason.into(),
    }
}

pub(super) fn no_parity_file_catalog_projection(
    object_id: &str,
    file: &RemTarFileLayout,
    prepared: &PreparedFile,
) -> Result<FileCatalogProjection, PoolWriteError> {
    let file_sha256 = file.file_sha256.ok_or_else(|| {
        PoolWriteError::InvalidInput(format!(
            "catalog projection supports regular files only, got {:?} for {}",
            file.entry_type, file.path
        ))
    })?;
    Ok(FileCatalogProjection {
        object_id: object_id.to_string(),
        file_id: file.file_id.clone(),
        path: file.path.clone(),
        size_bytes: file.size_bytes,
        file_sha256,
        first_chunk_lba: file.first_chunk_lba,
        chunk_count: file.chunk_count,
        mtime: prepared.spec.mtime.clone(),
        executable: file.executable,
    })
}
