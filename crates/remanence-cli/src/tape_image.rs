//! Shared streaming tape-image writer used by the live freeze drill and fixture generators.
//! Authorization and rewind belong to the caller. All byte choices are explicit;
//! replayable payload sources keep large drill writes bounded in memory.

use std::io::Read;
use std::sync::Arc;

use remanence_format::{
    plan_rem_tar_object, write_rem_tar_object_from_readers, RemTarFileSpec, RemTarFileStream,
    RemTarObjectOptions,
};
use remanence_library::DriveHandle;
use remanence_parity::{
    assemble_terminal_plan, checked_tape_index_replica_layout, index_separation_records,
    write_terminal_tail, CommittedBundle, CommittedBundleKind, CommittedState, DriveHandleRawSink,
    FilemarkMap, JournalError, ObjectRecoveryRepresentation, ParityError, ParityMapDiagnostics,
    ParityScheme, ParitySink, SidecarWriteSummary, TapeFileEntry, TapeFileJournal, TapeFileKind,
    TapeIndexReplicaCounts, TapeIndexReplicaFileKind, TapeIndexReplicaMapEntry,
    TapeIndexReplicaObjectRow, TapeIndexReplicaRecordSource, TapeIndexReplicaScope,
    TerminalComponentCommit, TerminalComponentReconcileEvidence, TerminalPrefixPlan,
    TerminalPrefixReconcileEvidence, TerminalTailAuthority, TerminalTailComponentPlan,
    TerminalTailLayout, TerminalTailProgress, TerminalTailRunOutcome,
    TerminalTripleCapacityRuntimeState, TerminalTripleCloseInput, TerminalTripleWritePlan,
    WriterIdentity,
};

/// A replayable file payload and all metadata passed to the REM-OBJECT writer.
#[derive(Clone)]
pub struct TapeImageFile {
    /// Complete file metadata, including the expected payload length and digest.
    pub spec: RemTarFileSpec,
    /// Each call must return the same bytes. The REM-OBJECT writer checks the
    /// length against `spec` on every run, and the digest only when
    /// `spec.file_sha256` is supplied.
    pub open: Arc<dyn Fn() -> Box<dyn Read> + Send + Sync>,
}

impl TapeImageFile {
    /// Supply literal bytes without requiring a streaming source factory.
    pub fn from_bytes(spec: RemTarFileSpec, bytes: Vec<u8>) -> Self {
        let bytes: Arc<[u8]> = bytes.into();
        Self {
            spec,
            open: Arc::new(move || Box::new(std::io::Cursor::new(Arc::clone(&bytes)))),
        }
    }
}

/// One Object's explicit REM-OBJECT options and ordered file inputs.
#[derive(Clone)]
pub struct TapeImageObject {
    /// All Object-level REM-OBJECT encoding options.
    pub options: RemTarObjectOptions,
    /// Ordered file metadata and replayable payload bytes.
    pub files: Vec<TapeImageFile>,
}

/// Every choice that can affect an image's bytes or checkpoint schedule.
/// Payloads remain replayable in the returned inputs, so generators can record
/// their bytes alongside the options without duplicating Object recovery rows.
#[derive(Clone)]
pub struct TapeImageInputs {
    /// Library-independent on-tape identity.
    pub tape_uuid: [u8; 16],
    /// Reed–Solomon and interleave geometry.
    pub scheme: ParityScheme,
    /// Fixed record size in bytes; compression is always disabled.
    pub block_size: u32,
    /// Ordered Objects written through the production format writer.
    pub objects: Vec<TapeImageObject>,
    /// Zero-based Object indices after which to issue a checkpoint.
    pub checkpoint_after_objects: Vec<usize>,
    /// Nominal bytes in each terminal separation extent.
    pub nominal_extent_bytes: u64,
    /// Bootstrap and terminal diagnostic identity; use a fixed clock for fixtures.
    pub writer_identity: WriterIdentity,
    /// Explicit terminal edition identity.
    pub edition_id: [u8; 16],
    /// Explicit terminal edition sequence.
    pub edition_sequence: u64,
    /// Initial value of the sink-owned ParityMap sequence counter.
    pub parity_map_sequence_start: u64,
    /// Primary/tail known-good hints; partial-epoch flags are geometry-derived.
    pub directory_flags: u32,
    /// Presence of optional ParityMap keys 6 and 7.
    pub diagnostic_keys_present: bool,
    /// Capacity basis for the existing production reservation calculator.
    pub capacity_bytes: u64,
}

/// Completed image plus original inputs and the actual captured diagnostics.
pub struct WrittenTapeImage {
    /// Original options and replayable byte sources, retained for recording.
    pub inputs: TapeImageInputs,
    /// Exact identity and timestamp encoded in the bootstrap.
    pub bootstrap_diagnostics: ParityMapDiagnostics,
    /// Exact diagnostic pair used by terminal planning.
    pub terminal_diagnostics: ParityMapDiagnostics,
    /// Recovery rows derived from the emitted REM-OBJECT layouts.
    pub object_rows: Vec<TapeIndexReplicaObjectRow>,
    /// Sidecars from Object close, checkpoints and terminal close, in tape order.
    pub sidecars: Vec<SidecarWriteSummary>,
    /// Complete committed physical file map.
    pub map: FilemarkMap,
    /// Shared assembly result used to write the terminal tail.
    pub terminal_plan: TerminalTripleWritePlan,
}

/// Write bootstrap, Objects, scheduled checkpoints, final prefix and terminal
/// tail through the production writers. No identity, UUID or schedule is chosen
/// here. The caller supplies a fresh/rewound, authorized tape.
pub fn write_tape_image(
    drive: &mut DriveHandle,
    inputs: TapeImageInputs,
) -> Result<WrittenTapeImage, String> {
    if inputs.block_size == 0
        || inputs
            .checkpoint_after_objects
            .iter()
            .any(|&i| i >= inputs.objects.len())
        || inputs
            .checkpoint_after_objects
            .windows(2)
            .any(|w| w[0] >= w[1])
    {
        return Err("invalid tape image block size or checkpoint schedule".to_string());
    }
    index_separation_records(inputs.block_size, inputs.nominal_extent_bytes)
        .map_err(|error| error.to_string())?;
    let bootstrap_diagnostics = inputs
        .writer_identity
        .capture()
        .map_err(|error| error.to_string())?;
    let bootstrap_instant = time::OffsetDateTime::parse(
        &bootstrap_diagnostics.write_timestamp,
        &time::format_description::well_known::Rfc3339,
    )
    .map_err(|error| error.to_string())?;
    let mut raw = DriveHandleRawSink::new(drive);
    raw.configure_parity_write_session(inputs.block_size)
        .map_err(|error| format!("configure parity write session: {error}"))?;
    let mut journal = ImageJournal::new(inputs.tape_uuid);
    let mut parity = ParitySink::new_with_journal(
        &mut raw,
        &mut journal,
        inputs.scheme.clone(),
        inputs.tape_uuid,
        inputs.block_size,
        WriterIdentity::fixed(
            bootstrap_diagnostics.writer_version.clone(),
            bootstrap_instant,
        ),
    )
    .map_err(|error| format!("open parity write sink: {error}"))?;
    parity
        .configure_parity_map_encoding(
            inputs.parity_map_sequence_start,
            inputs.directory_flags,
            inputs.diagnostic_keys_present,
        )
        .map_err(|error| error.to_string())?;
    parity
        .write_bootstrap()
        .map_err(|error| format!("write BOT bootstrap: {error}"))?;
    let mut object_rows = Vec::with_capacity(inputs.objects.len());
    let mut sidecars = Vec::new();
    for (object_index, object) in inputs.objects.iter().enumerate() {
        if object.options.chunk_size != inputs.block_size as usize {
            return Err(format!(
                "Object {object_index} chunk size differs from tape block size"
            ));
        }
        let specs: Vec<_> = object.files.iter().map(|file| file.spec.clone()).collect();
        let layout = plan_rem_tar_object(&object.options, &specs)
            .map_err(|error| format!("plan Object {object_index}: {error}"))?;
        let runtime = parity
            .terminal_triple_capacity_runtime_state()
            .map_err(|error| error.to_string())?;
        let reserve = capacity_input(
            &inputs.scheme,
            inputs.block_size,
            layout.projected_size_blocks,
            runtime,
            inputs.capacity_bytes,
        )?
        .reserve_object()
        .map_err(|error| format!("reserve Object {object_index}: {error}"))?;
        let opened = parity
            .begin_object_with_terminal_triple_reservation(reserve)
            .map_err(|error| format!("admit Object {object_index}: {error}"))?;
        let mut readers: Vec<_> = object.files.iter().map(|file| (file.open)()).collect();
        let mut streams: Vec<_> = specs
            .into_iter()
            .zip(readers.iter_mut())
            .map(|(spec, reader)| RemTarFileStream::new(spec, reader.as_mut()))
            .collect();
        let written_layout =
            write_rem_tar_object_from_readers(&mut parity, &object.options, &mut streams)
                .map_err(|error| format!("write Object {object_index}: {error}"))?;
        let recovery_row = TapeIndexReplicaObjectRow {
            tape_file_number: opened.0,
            stored_block_count: written_layout.projected_size_blocks,
            object_id: object.options.object_id.as_bytes().to_vec(),
            representation: ObjectRecoveryRepresentation::Plaintext {
                manifest_first_chunk_lba: written_layout
                    .manifest
                    .first_chunk_lba
                    .ok_or_else(|| {
                        format!("Object {object_index} manifest has no first chunk LBA")
                    })?
                    .0,
                manifest_size_bytes: written_layout.manifest.size_bytes,
                manifest_chunk_count: written_layout.manifest.chunk_count,
                manifest_sha256: written_layout.manifest_sha256,
            },
        };
        let closed = parity
            .finish_object()
            .map_err(|error| format!("close Object {object_index}: {error}"))?;
        if recovery_row.stored_block_count != closed.data_block_count {
            return Err(format!(
                "Object {object_index} recovery row differs from written block count"
            ));
        }
        object_rows.push(recovery_row);
        sidecars.extend(closed.sidecars_emitted);
        if inputs.checkpoint_after_objects.contains(&object_index) {
            let checkpoint = parity
                .checkpoint()
                .map_err(|error| format!("write intermediate checkpoint: {error}"))?;
            sidecars.extend(checkpoint.sidecars_emitted);
        }
    }
    let terminal_diagnostics = inputs
        .writer_identity
        .capture()
        .map_err(|error| error.to_string())?;
    let prefix_plan = parity
        .plan_terminal_index_close(terminal_diagnostics.clone())
        .map_err(|error| format!("plan terminal prefix: {error}"))?;
    let closed = parity
        .close_for_terminal_index(&prefix_plan, TerminalPrefixReconcileEvidence::Absent)
        .map_err(|error| format!("write terminal prefix: {error}"))?;
    sidecars.extend(closed.sidecars_emitted);
    let committed = journal
        .load_committed()
        .map_err(|error| error.to_string())?;
    if !committed.orphaned_bundles.is_empty() {
        return Err("image journal retained orphaned bundles after final checkpoint".to_string());
    }
    let (mut terminal_rows, terminal_plan) = plan_image_terminal_tail(
        inputs.tape_uuid,
        inputs.block_size,
        inputs.nominal_extent_bytes,
        inputs.edition_id,
        inputs.edition_sequence,
        &prefix_plan,
        &committed,
        &object_rows,
    )?;
    let mut authority = ImageTerminalAuthority {
        journal: &mut journal,
        progress: TerminalTailProgress::BeforeReplicaA,
    };
    match write_terminal_tail(&mut raw, &mut terminal_rows, &mut authority, &terminal_plan)
        .map_err(|error| format!("write terminal A/gap/B/gap/C tail: {error}"))?
    {
        TerminalTailRunOutcome::Complete => {}
        outcome => {
            return Err(format!(
                "fresh image terminal write requires recovery: {outcome:?}"
            ))
        }
    }
    let committed = journal
        .load_committed()
        .map_err(|error| error.to_string())?;
    let map = committed
        .filemark_map()
        .map_err(|error| error.to_string())?;
    Ok(WrittenTapeImage {
        inputs,
        bootstrap_diagnostics,
        terminal_diagnostics,
        object_rows,
        sidecars,
        map,
        terminal_plan,
    })
}

struct ImageJournal {
    tape_uuid: [u8; 16],
    bundles: Vec<CommittedBundle>,
}

impl ImageJournal {
    fn new(tape_uuid: [u8; 16]) -> Self {
        Self {
            tape_uuid,
            bundles: Vec::new(),
        }
    }
}

impl TapeFileJournal for ImageJournal {
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

#[derive(Clone)]
struct ImageTerminalRows {
    entries: Vec<TapeIndexReplicaMapEntry>,
    object_rows: Vec<TapeIndexReplicaObjectRow>,
}

impl TapeIndexReplicaRecordSource for ImageTerminalRows {
    fn visit_structural_entries(
        &mut self,
        visitor: &mut dyn FnMut(&TapeIndexReplicaMapEntry) -> Result<(), ParityError>,
    ) -> Result<(), ParityError> {
        for entry in &self.entries {
            visitor(entry)?;
        }
        Ok(())
    }

    fn visit_object_rows(
        &mut self,
        visitor: &mut dyn FnMut(&TapeIndexReplicaObjectRow) -> Result<(), ParityError>,
    ) -> Result<(), ParityError> {
        for row in &self.object_rows {
            visitor(row)?;
        }
        Ok(())
    }
}

struct ImageTerminalAuthority<'a> {
    journal: &'a mut ImageJournal,
    progress: TerminalTailProgress,
}

impl TerminalTailAuthority for ImageTerminalAuthority<'_> {
    fn load_progress(&mut self) -> Result<TerminalTailProgress, String> {
        Ok(self.progress)
    }

    fn reconcile_next(
        &mut self,
        progress: TerminalTailProgress,
        _component: TerminalTailComponentPlan,
    ) -> Result<TerminalComponentReconcileEvidence, String> {
        if progress != self.progress {
            return Err("tape-image terminal progress changed before reconciliation".to_string());
        }
        Ok(TerminalComponentReconcileEvidence::Absent)
    }

    fn commit_after_barrier(&mut self, commit: &TerminalComponentCommit) -> Result<(), String> {
        if commit.previous_progress != self.progress {
            return Err("tape-image terminal commit used stale progress".to_string());
        }
        self.journal
            .commit_terminal_component_transition(&commit.journal_bundle, &commit.checkpoint_bundle)
            .map_err(|error| error.to_string())?;
        self.progress = commit.next_progress;
        Ok(())
    }
}

fn capacity_input(
    scheme: &ParityScheme,
    block_size: u32,
    projected_object_blocks: u64,
    runtime: TerminalTripleCapacityRuntimeState,
    capacity_bytes: u64,
) -> Result<TerminalTripleCloseInput, String> {
    let capacity_blocks = capacity_bytes / u64::from(block_size);
    let remaining_tape_blocks = capacity_blocks
        .checked_sub(runtime.used_tape_blocks)
        .ok_or_else(|| {
            format!(
                "tape-image physical cursor {} exceeds conservative capacity basis {capacity_blocks}",
                runtime.used_tape_blocks
            )
        })?;
    let low_watermark_blocks = capacity_blocks.saturating_mul(92) / 100;
    let high_watermark_blocks = capacity_blocks.saturating_mul(97) / 100;
    Ok(TerminalTripleCloseInput {
        projected_object_present: true,
        projected_object_blocks,
        block_size_bytes: block_size,
        current_epoch_fill_blocks: runtime.current_epoch_fill_blocks,
        data_shards_per_epoch: u64::from(scheme.data_blocks_per_stripe)
            * u64::from(scheme.stripes_per_neighborhood),
        parity_shards_per_epoch: u64::from(scheme.parity_blocks_per_stripe)
            * u64::from(scheme.stripes_per_neighborhood),
        pending_completed_sidecars: runtime.pending_completed_sidecars,
        sidecar_entries_before_object: runtime.sidecar_entries_before_object,
        structural_entries_before_object: runtime.structural_entries_before_object,
        object_rows_before_object: runtime.object_rows_before_object,
        object_filemark_blocks: 1,
        sidecar_filemark_blocks: 1,
        parity_map_filemark_blocks: 1,
        replica_filemark_blocks: 1,
        gap_filemark_blocks: 1,
        // Production admission conservatively reserves the default extents,
        // including when the image writes the compact fixture geometry.
        gap_nominal_bytes: remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
        safety_margin_blocks: 4,
        remaining_tape_blocks,
        capacity_basis_blocks: capacity_blocks,
        low_watermark_blocks,
        high_watermark_blocks,
        pending_completed_epoch_parity_bytes: runtime.pending_completed_epoch_parity_bytes,
        remaining_spool_bytes: u64::MAX,
    })
}

fn terminal_map_entry(entry: &TapeFileEntry) -> TapeIndexReplicaMapEntry {
    TapeIndexReplicaMapEntry {
        tape_file_number: entry.tape_file_number,
        kind: match entry.kind {
            TapeFileKind::Object => TapeIndexReplicaFileKind::Object,
            TapeFileKind::ParitySidecar => TapeIndexReplicaFileKind::ParitySidecar,
            TapeFileKind::Bootstrap => TapeIndexReplicaFileKind::Bootstrap,
            TapeFileKind::ParityMap => TapeIndexReplicaFileKind::ParityMap,
            TapeFileKind::TapeIndexReplica => TapeIndexReplicaFileKind::TapeIndexReplica,
            TapeFileKind::IndexSeparationExtent => TapeIndexReplicaFileKind::IndexSeparationExtent,
        },
        block_count: entry.block_count,
        first_parity_data_ordinal: entry.first_parity_data_ordinal,
        protected_ordinal_start: entry.protected_ordinal_start,
        protected_ordinal_end_exclusive: entry.protected_ordinal_end_exclusive,
        epoch_id: entry.epoch_id,
    }
}

#[allow(clippy::too_many_arguments)]
fn plan_image_terminal_tail(
    tape_uuid: [u8; 16],
    block_size: u32,
    nominal_extent_bytes: u64,
    edition_id: [u8; 16],
    edition_sequence: u64,
    prefix: &TerminalPrefixPlan,
    committed: &CommittedState,
    object_rows: &[TapeIndexReplicaObjectRow],
) -> Result<(ImageTerminalRows, TerminalTripleWritePlan), String> {
    let mut rows = ImageTerminalRows {
        entries: committed.entries.iter().map(terminal_map_entry).collect(),
        object_rows: object_rows.to_vec(),
    };
    let structural_entry_count = u64::try_from(rows.entries.len())
        .map_err(|_| "tape-image structural row count exceeds u64::MAX".to_string())?;
    if structural_entry_count != prefix.tail_start_tape_file_number {
        return Err(format!(
            "tape-image terminal prefix has {structural_entry_count} rows, expected {}",
            prefix.tail_start_tape_file_number
        ));
    }
    let counts = TapeIndexReplicaCounts {
        structural_entry_count,
        object_row_count: u64::try_from(rows.object_rows.len())
            .map_err(|_| "tape-image Object row count exceeds u64::MAX".to_string())?,
    };
    let replica_records = checked_tape_index_replica_layout(block_size, counts)
        .map_err(|error| format!("plan tape-image terminal replica layout: {error}"))?
        .replica_record_count;
    let layout = TerminalTailLayout::new(
        0,
        block_size,
        structural_entry_count,
        prefix.tail_start_lba,
        replica_records,
        index_separation_records(block_size, nominal_extent_bytes)
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("plan tape-image terminal tail layout: {error}"))?;
    let plan = assemble_terminal_plan(
        tape_uuid,
        block_size,
        false,
        edition_sequence,
        TapeIndexReplicaScope {
            covered_prefix_tape_file_count: structural_entry_count,
            total_data_ordinals: committed.total_committed_ordinals,
            highest_protected_ordinal: committed.highest_protected_ordinal,
        },
        counts,
        &mut rows,
        layout,
        prefix.diagnostics.clone(),
        edition_id,
        nominal_extent_bytes,
    )
    .map_err(|error| format!("assemble terminal image plan: {error}"))?;
    Ok((rows, plan))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Mutex;

    use remanence_chaos::model::{
        DeviceRole, ExportedTapeImage, ModelTransport, VirtualTape, VirtualWorld,
    };
    use remanence_parity::{
        default_scheme_for_block_size, parse_parity_map_tape_file,
        SIDECAR_DIRECTORY_FLAG_FINAL_PARTIAL_EPOCH, SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD,
        SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD,
    };
    use sha2::{Digest, Sha256};

    const BLOCK: u32 = 262_144;

    fn fixed_inputs(with_payload: bool, checkpoint: bool, diagnostics: bool) -> TapeImageInputs {
        let mut scheme = default_scheme_for_block_size(BLOCK);
        scheme.data_blocks_per_stripe = 2;
        scheme.parity_blocks_per_stripe = 2;
        scheme.stripes_per_neighborhood = 2;
        let mut options = RemTarObjectOptions::new(
            "00000000-0000-4000-8000-000000000001",
            "image-object",
            "2026-01-01T00:00:00Z",
            "00000000-0000-4000-8000-000000000002",
        );
        options.chunk_size = BLOCK as usize;
        let files = if with_payload {
            let bytes = vec![0x35; BLOCK as usize];
            vec![TapeImageFile::from_bytes(
                RemTarFileSpec::new(
                    "payload.bin",
                    "00000000-0000-4000-8000-000000000003",
                    bytes.len() as u64,
                    Sha256::digest(&bytes).into(),
                ),
                bytes,
            )]
        } else {
            vec![]
        };
        TapeImageInputs {
            tape_uuid: *uuid::Uuid::parse_str("12345678-1234-4234-8234-123456789abc")
                .unwrap()
                .as_bytes(),
            scheme,
            block_size: BLOCK,
            objects: vec![TapeImageObject { options, files }],
            checkpoint_after_objects: if checkpoint { vec![0] } else { vec![] },
            nominal_extent_bytes: 3 * u64::from(BLOCK),
            writer_identity: WriterIdentity::fixed(
                "image-test/1".to_string(),
                time::OffsetDateTime::UNIX_EPOCH,
            ),
            edition_id: [0x54; 16],
            edition_sequence: 7,
            parity_map_sequence_start: 11,
            directory_flags: if diagnostics {
                SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD | SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD
            } else {
                0
            },
            diagnostic_keys_present: diagnostics,
            capacity_bytes: 6_000_000_000_000,
        }
    }

    fn write_model(inputs: TapeImageInputs) -> (WrittenTapeImage, ExportedTapeImage) {
        let mut world = VirtualWorld::single_drive("IMAGE-LIB", 0x100, "IMAGE-DRV", 0x400, 1);
        world.put_tape_in_drive(
            0x100,
            "IMAGE001",
            None,
            VirtualTape::empty(64 * 1024 * 1024, BLOCK),
        );
        let world = Arc::new(Mutex::new(world));
        let transport = ModelTransport::new(Arc::clone(&world), DeviceRole::Drive { bay: 0x100 });
        let mut drive = DriveHandle::open_standalone_with_transport(
            Path::new("/dev/sg-image-model"),
            Box::new(transport),
        )
        .expect("open model drive");
        let written = write_tape_image(&mut drive, inputs).expect("write fixed image");
        let exported = world.lock().unwrap().tapes["IMAGE001"].export_image();
        (written, exported)
    }

    #[test]
    fn tape_image_is_deterministic_and_collects_every_sidecar_emission_path() {
        // Full epoch: finish_object emits. Partial epoch: checkpoint or terminal close emits.
        for (payload, checkpoint, diagnostics, expected_sha256) in [
            (
                true,
                true,
                true,
                "1eee56d263b1a21154b2e43778d1db3c88bb77a7d8d355d002f475b660193946",
            ),
            (
                false,
                true,
                false,
                "d4eab7bce70d2296953c6ac010c87bd3d23619cca9a31e8af66ed8739eff1b1c",
            ),
            (
                false,
                false,
                true,
                "623dc9f0d3587c13847f4df67f18c161ed7772cfd8469c4c4d598a707dfa8f90",
            ),
        ] {
            let inputs = fixed_inputs(payload, checkpoint, diagnostics);
            let (first, first_bytes) = write_model(inputs.clone());
            let (second, second_bytes) = write_model(inputs);
            assert_eq!(first_bytes, second_bytes);
            // Pin the concatenated exported file bytes in physical tape order.
            let mut digest = Sha256::new();
            for file in &first_bytes.files {
                digest.update(&file.bytes);
            }
            assert_eq!(format!("{:x}", digest.finalize()), expected_sha256);
            assert_eq!(first.terminal_plan, second.terminal_plan);
            assert_eq!(first.sidecars, second.sidecars);
            assert_eq!(first.sidecars.len(), 1);
            assert_eq!(
                first.object_rows[0].stored_block_count,
                if payload { 4 } else { 2 }
            );
            assert_eq!(
                first_bytes.eod_record as u64,
                first
                    .terminal_plan
                    .edition
                    .descriptor
                    .terminal_layout
                    .expected_eod_lba
            );
            assert_eq!(
                first.inputs.edition_id,
                first.terminal_plan.edition.descriptor.edition_id
            );
            assert_eq!(first.bootstrap_diagnostics, first.terminal_diagnostics);
            assert_eq!(
                first.terminal_diagnostics.write_timestamp,
                "1970-01-01T00:00:00Z"
            );
            assert_eq!(
                first.terminal_plan.edition.descriptor.write_timestamp,
                "1970-01-01T00:00:00Z"
            );
            let bootstrap =
                remanence_parity::bootstrap::parse_bootstrap_block(&first_bytes.files[0].bytes)
                    .expect("decode emitted bootstrap");
            assert_eq!(bootstrap.written_at, "1970-01-01T00:00:00Z");
            let map_file = first
                .map
                .entries()
                .iter()
                .find(|entry| entry.kind == TapeFileKind::ParityMap)
                .expect("final ParityMap")
                .tape_file_number;
            let blocks: Vec<Vec<u8>> = first_bytes.files[map_file as usize]
                .bytes
                .chunks_exact(BLOCK as usize)
                .map(|block| block.to_vec())
                .collect();
            let decoded = parse_parity_map_tape_file(&blocks, &first.inputs.tape_uuid)
                .expect("decode emitted map");
            assert_eq!(decoded.payload.sequence, 11);
            assert_eq!(
                decoded.payload.writer_version.as_deref(),
                diagnostics.then_some("image-test/1")
            );
            assert_eq!(
                decoded.payload.write_timestamp.as_deref(),
                diagnostics.then_some("1970-01-01T00:00:00Z")
            );
            let expected_flags = first.inputs.directory_flags
                | if payload || checkpoint {
                    0
                } else {
                    SIDECAR_DIRECTORY_FLAG_FINAL_PARTIAL_EPOCH
                };
            assert_eq!(decoded.payload.directory.entries[0].flags, expected_flags);
        }
    }

    /// Reload production-written bytes into the chaos model with physical READ faults.
    fn damaged_image_drive(
        image: &ExportedTapeImage,
        faults: Vec<u64>,
    ) -> (DriveHandle, remanence_chaos::FaultEngine) {
        use remanence_chaos::model::Record;
        use remanence_chaos::{ChaosTransport, DeviceCtx, FaultEngine};
        let mut tape = VirtualTape::empty(64 * 1024 * 1024, BLOCK);
        for file in &image.files {
            assert_eq!(tape.records.len(), file.start_record);
            for block in file.bytes.chunks_exact(BLOCK as usize) {
                tape.records.push(Record::Block(block.to_vec()));
            }
            assert_eq!(file.filemark_record, Some(tape.records.len()));
            tape.records.push(Record::Filemark);
        }
        assert_eq!(tape.records.len(), image.eod_record);
        let mut world = VirtualWorld::single_drive("IMAGE-LIB", 0x100, "IMAGE-DRV", 0x400, 1);
        world.put_tape_in_drive(0x100, "IMAGE001", None, tape);
        let transport = ModelTransport::new(
            Arc::new(Mutex::new(world)),
            DeviceRole::Drive { bay: 0x100 },
        );
        let engine = FaultEngine::for_read_medium_errors(faults).unwrap();
        let transport = ChaosTransport::new(
            transport,
            engine.clone(),
            DeviceCtx::new().with_backend("model"),
        );
        let drive = DriveHandle::open_standalone_with_transport(
            Path::new("/dev/sg-image-model"),
            Box::new(transport),
        )
        .expect("open damaged image");
        (drive, engine)
    }

    /// Exercise Q4 on actual writer bytes, with a failed data read and lost
    /// sidecar primary/footer. Directory mutations retain valid ParityMap CRCs.
    fn directory_rescue_case(walk: bool, damage: &str) {
        use remanence_parity::{
            encode_parity_map_tape_file, read_terminal_index_inventory,
            recover_ordinal_from_sidecar, recover_terminal_inventory_from_bot,
            scan_reconstruct_filemark_map_with_report, validate_scan_reconstruction_with_report,
            DriveHandleRawSource, PhysicalPositionHint, RawTapeSource, ScopedFilemarkMap,
            SidecarMetadataHealth, TapeFileMapEntry, TapeIndexReplicaFileKind,
            TerminalInventoryOutcome,
        };
        let (written, mut image) = write_model(fixed_inputs(true, true, true));
        let uuid = written.inputs.tape_uuid;
        let sidecar_file = 2usize;
        let map_file = 3usize;
        let sidecar_start = image.files[sidecar_file].start_record as u64;
        let sidecar_count = image.files[sidecar_file].record_offsets.len() as u64;
        let map_start = image.files[map_file].start_record as u64;
        let map_blocks: Vec<_> = image.files[map_file]
            .bytes
            .chunks_exact(BLOCK as usize)
            .map(|block| block.to_vec())
            .collect();
        let decoded_map = parse_parity_map_tape_file(&map_blocks, &uuid).unwrap();
        let map_tail_start = map_start + decoded_map.header.tail_copy_start_block;
        let mut payload = decoded_map.payload.clone();
        let tail_start = sidecar_start
            + payload.directory.entries[0].sidecar_header_block_count
            + payload.directory.entries[0].parity_shard_block_count;
        let mut faults = vec![sidecar_start, sidecar_start + sidecar_count - 1];
        let data_start = image.files[1].start_record as u64;
        faults.push(data_start);
        match damage {
            "none" => {}
            "map-primary" => faults.push(map_start),
            "map-both" => faults.extend([map_start, map_tail_start]),
            "range" => {
                payload.directory.entries[0].protected_ordinal_end_exclusive -= 1;
                payload.directory.directory_scope_highest_protected_ordinal -= 1;
            }
            "count" => payload.directory.entries[0].sidecar_total_block_count += 1,
            "hash" => payload.directory.entries[0].canonical_metadata_hash[0] ^= 1,
            _ => panic!("unknown damage case"),
        }
        if matches!(damage, "range" | "count" | "hash") {
            let encoded = encode_parity_map_tape_file(&payload, BLOCK).unwrap();
            assert_eq!(encoded.blocks.len(), map_blocks.len());
            image.files[map_file].bytes = encoded.blocks.concat();
            // Prove that map disagreements reject the locator before a tail read.
            if damage != "hash" {
                faults.push(tail_start);
            }
        }
        if walk {
            for entry in written
                .map
                .entries()
                .iter()
                .filter(|entry| entry.kind == TapeFileKind::TapeIndexReplica)
            {
                let file = &image.files[entry.tape_file_number as usize];
                faults.extend(
                    (file.start_record as u64)..(file.start_record as u64 + entry.block_count),
                );
            }
        }
        let (mut drive, engine) = damaged_image_drive(&image, faults);
        let mut raw = DriveHandleRawSource::new(&mut drive);
        raw.configure_fixed_block_size(BLOCK).unwrap();
        raw.locate_physical(PhysicalPositionHint::new(data_start))
            .unwrap();
        assert!(
            raw.read_record(&mut vec![0; BLOCK as usize]).is_err(),
            "recovery subject must fail to read"
        );
        let mut rows = Vec::new();
        let outcome = read_terminal_index_inventory(
            &mut raw,
            &uuid,
            BLOCK,
            |row| {
                rows.push(row.clone());
                Ok(())
            },
            |_| Ok(()),
        )
        .unwrap();
        let scoped = if walk {
            assert!(matches!(
                outcome,
                TerminalInventoryOutcome::BotStructuralRecoveryRequired(_)
            ));
            assert!(rows.is_empty());
            let walked = scan_reconstruct_filemark_map_with_report(&mut raw, &uuid, BLOCK).unwrap();
            assert!(
                walked.map.tape_file_count() > payload.directory.directory_scope_tape_file_count
            );
            let expected_kind = if damage == "count" {
                TapeFileKind::Object
            } else {
                TapeFileKind::ParitySidecar
            };
            assert_eq!(walked.map.entries()[sidecar_file].kind, expected_kind);
            let mut objects = Vec::new();
            recover_terminal_inventory_from_bot(&mut raw, &uuid, BLOCK, |object| {
                objects.push(object.tape_file_number);
                Ok(())
            })
            .unwrap();
            assert_eq!(
                objects.contains(&(sidecar_file as u64)),
                expected_kind == TapeFileKind::Object
            );
            if expected_kind == TapeFileKind::Object {
                assert!(matches!(
                    ScopedFilemarkMap::validate_against_final_parity_map(walked.map, &decoded_map),
                    Err(ParityError::FilemarkMapDigestMismatch { .. })
                ));
                return;
            }
            let bootstrap = walked.authoritative_bootstrap().unwrap().payload.clone();
            let current_blocks: Vec<_> = image.files[map_file]
                .bytes
                .chunks_exact(BLOCK as usize)
                .map(|block| block.to_vec())
                .collect();
            let current_map = parse_parity_map_tape_file(&current_blocks, &uuid).unwrap();
            let expected = ScopedFilemarkMap::validate_against_final_parity_map(
                walked.map.clone(),
                &current_map,
            )
            .unwrap();
            let acquired = validate_scan_reconstruction_with_report(&mut raw, &bootstrap, walked)
                .unwrap()
                .scoped_map;
            assert_eq!(acquired, expected);
            assert_eq!(
                acquired.validated_prefix_tape_files,
                Some(payload.directory.directory_scope_tape_file_count)
            );
            assert_eq!(
                acquired.sidecar_directory.as_ref(),
                Some(&payload.directory)
            );
            acquired
        } else {
            assert!(matches!(outcome, TerminalInventoryOutcome::Inventory(_)));
            let map = FilemarkMap::new(
                rows.iter()
                    .map(|row| TapeFileMapEntry {
                        tape_file_number: row.tape_file_number,
                        kind: match row.kind {
                            TapeIndexReplicaFileKind::Bootstrap => TapeFileKind::Bootstrap,
                            TapeIndexReplicaFileKind::Object => TapeFileKind::Object,
                            TapeIndexReplicaFileKind::ParitySidecar => TapeFileKind::ParitySidecar,
                            TapeIndexReplicaFileKind::ParityMap => TapeFileKind::ParityMap,
                            TapeIndexReplicaFileKind::TapeIndexReplica => {
                                TapeFileKind::TapeIndexReplica
                            }
                            TapeIndexReplicaFileKind::IndexSeparationExtent => {
                                TapeFileKind::IndexSeparationExtent
                            }
                        },
                        block_count: row.block_count,
                        first_parity_data_ordinal: row.first_parity_data_ordinal,
                        protected_ordinal_start: row.protected_ordinal_start,
                        protected_ordinal_end_exclusive: row.protected_ordinal_end_exclusive,
                        epoch_id: row.epoch_id,
                    })
                    .collect(),
            )
            .unwrap();
            let scoped = ScopedFilemarkMap::from_catalog(map, 4);
            assert!(
                scoped.sidecar_directory.is_none(),
                "exercise lazy loading, not an injected directory"
            );
            scoped
        };
        let result =
            recover_ordinal_from_sidecar(&mut raw, &scoped, &written.inputs.scheme, uuid, BLOCK, 0);
        if matches!(damage, "map-both" | "range" | "count" | "hash") {
            assert!(
                matches!(
                    result,
                    Err(ParityError::SidecarMetadataUnavailable { epoch_id: 0 })
                ),
                "{result:?}"
            );
            if matches!(damage, "range" | "count") {
                assert!(
                    !engine.observed_medium_error_lbas().contains(&tail_start),
                    "disagreeing directory must not place a tail read"
                );
            }
        } else {
            let recovered = result.expect("directory must rescue the sidecar tail");
            assert_eq!(
                recovered.recovered_block,
                image.files[1].bytes[..BLOCK as usize]
            );
            assert_eq!(
                recovered.sidecar_metadata_health,
                SidecarMetadataHealth::PrimaryHeaderLost
            );
            let observed = engine.observed_medium_error_lbas();
            assert!(observed.contains(&data_start));
            assert!(observed.contains(&sidecar_start));
            assert!(observed.contains(&(sidecar_start + sidecar_count - 1)));
        }
    }

    #[test]
    fn directory_rescue_lazily_loads_from_replica_rows() {
        directory_rescue_case(false, "none");
        directory_rescue_case(false, "map-primary");
    }

    #[test]
    fn directory_rescue_reconciles_bot_walk_without_any_replica() {
        directory_rescue_case(true, "none");
    }

    #[test]
    fn directory_rescue_rejects_map_disagreement_before_tail_read() {
        directory_rescue_case(false, "range");
        directory_rescue_case(false, "count");
        directory_rescue_case(true, "count");
    }

    #[test]
    fn directory_rescue_requires_a_valid_parity_map_copy() {
        directory_rescue_case(false, "map-both");
    }

    #[test]
    fn directory_rescue_requires_the_directory_metadata_hash() {
        directory_rescue_case(false, "hash");
        directory_rescue_case(true, "hash");
    }

    /// A wholly unreadable epoch's metadata cannot deny another epoch the walked
    /// map or recovery. Build the two-epoch shape through the production writer.
    #[test]
    fn directory_walk_isolates_wholly_unreadable_epoch_metadata() {
        use remanence_parity::{
            read_terminal_index_inventory, recover_ordinal_from_sidecar,
            scan_reconstruct_filemark_map_with_report, validate_scan_reconstruction_with_report,
            DriveHandleRawSource, PhysicalPositionHint, RawTapeSource, TerminalInventoryOutcome,
        };
        let mut inputs = fixed_inputs(true, true, true);
        let bytes = vec![0x35; 5 * BLOCK as usize];
        inputs.objects[0].files = vec![TapeImageFile::from_bytes(
            RemTarFileSpec::new(
                "payload.bin",
                "00000000-0000-4000-8000-000000000003",
                bytes.len() as u64,
                Sha256::digest(&bytes).into(),
            ),
            bytes,
        )];
        let (written, image) = write_model(inputs);
        assert_eq!(written.object_rows.len(), 1);
        assert_eq!(written.object_rows[0].stored_block_count, 8);
        assert_eq!(written.sidecars.len(), 2);
        let uuid = written.inputs.tape_uuid;
        let map_file = written
            .map
            .entries()
            .iter()
            .find(|entry| entry.kind == TapeFileKind::ParityMap)
            .unwrap();
        let blocks: Vec<_> = image.files[map_file.tape_file_number as usize]
            .bytes
            .chunks_exact(BLOCK as usize)
            .map(|block| block.to_vec())
            .collect();
        let decoded = parse_parity_map_tape_file(&blocks, &uuid).unwrap();
        let directory = &decoded.payload.directory;
        assert_eq!(directory.entries.len(), 2);
        let epoch = &directory.entries[0];
        assert_eq!(epoch.epoch_id, 0);
        assert_eq!(epoch.protected_ordinal_start, 0);
        assert_eq!(epoch.protected_ordinal_end_exclusive, 4);
        assert_eq!(directory.entries[1].protected_ordinal_start, 4);
        assert_eq!(directory.entries[1].protected_ordinal_end_exclusive, 8);
        let sidecar_start = image.files[epoch.tape_file_number as usize].start_record as u64;
        let tail_start =
            sidecar_start + epoch.sidecar_total_block_count - 1 - epoch.sidecar_header_block_count;
        let footer = sidecar_start + epoch.sidecar_total_block_count - 1;
        let mut faults = Vec::new();
        faults.extend(sidecar_start..sidecar_start + epoch.sidecar_header_block_count);
        faults.extend(tail_start..=footer);
        for entry in written
            .map
            .entries()
            .iter()
            .filter(|entry| entry.kind == TapeFileKind::TapeIndexReplica)
        {
            let start = image.files[entry.tape_file_number as usize].start_record as u64;
            faults.extend(start..start + entry.block_count);
        }
        let data_start = image.files[1].start_record as u64;
        faults.extend([data_start, data_start + 4]);
        let (mut drive, engine) = damaged_image_drive(&image, faults);
        let mut raw = DriveHandleRawSource::new(&mut drive);
        raw.configure_fixed_block_size(BLOCK).unwrap();
        for lba in [data_start, data_start + 4] {
            raw.locate_physical(PhysicalPositionHint::new(lba)).unwrap();
            assert!(
                raw.read_record(&mut vec![0; BLOCK as usize]).is_err(),
                "each epoch's recovery subject must fail to read"
            );
        }
        let outcome = read_terminal_index_inventory(
            &mut raw,
            &uuid,
            BLOCK,
            |_| panic!("unreadable replicas cannot supply inventory rows"),
            |_| Ok(()),
        )
        .unwrap();
        assert!(matches!(
            outcome,
            TerminalInventoryOutcome::BotStructuralRecoveryRequired(_)
        ));
        let walked = scan_reconstruct_filemark_map_with_report(&mut raw, &uuid, BLOCK).unwrap();
        let scope = directory.directory_scope_tape_file_count as usize;
        assert_eq!(
            &walked.map.entries()[..scope],
            &written.map.entries()[..scope]
        );
        let bootstrap = walked.authoritative_bootstrap().unwrap().payload.clone();
        let scoped = validate_scan_reconstruction_with_report(&mut raw, &bootstrap, walked)
            .unwrap()
            .scoped_map;
        assert_eq!(
            scoped.validated_prefix_tape_files,
            Some(directory.directory_scope_tape_file_count)
        );
        assert_eq!(scoped.scope.watermark(), 8);
        assert_eq!(scoped.sidecar_directory.as_ref(), Some(directory));
        assert!(
            !engine.observed_medium_error_lbas().contains(&tail_start),
            "walk identification and digest validation must not read the sidecar tail"
        );
        assert!(matches!(
            recover_ordinal_from_sidecar(&mut raw, &scoped, &written.inputs.scheme, uuid, BLOCK, 0),
            Err(ParityError::SidecarMetadataUnavailable { epoch_id: 0 })
        ));
        let observed = engine.observed_medium_error_lbas();
        for lba in [sidecar_start, tail_start, footer] {
            assert!(
                observed.contains(&lba),
                "epoch 0 metadata fault must be exercised"
            );
        }
        let recovered =
            recover_ordinal_from_sidecar(&mut raw, &scoped, &written.inputs.scheme, uuid, BLOCK, 4)
                .expect("epoch 1 must recover despite epoch 0 metadata damage");
        assert_eq!(
            recovered.recovered_block,
            image.files[1].bytes[4 * BLOCK as usize..5 * BLOCK as usize]
        );
    }

    /// Reclassification removes sidecar blocks from the Object ordinal stream
    /// while preserving every other measured classification, including sidecars.
    #[test]
    fn directory_rescue_walk_recomputes_later_object_ordinals() {
        use remanence_parity::{scan_reconstruct_filemark_map_with_report, DriveHandleRawSource};
        let mut inputs = fixed_inputs(true, true, true);
        let mut second = inputs.objects[0].clone();
        second.options.object_id = "00000000-0000-4000-8000-000000000004".to_string();
        inputs.objects.push(second);
        let (written, image) = write_model(inputs);
        assert_eq!(written.sidecars.len(), 2);
        let sidecar = &image.files[2];
        let faults = vec![
            sidecar.start_record as u64,
            (sidecar.start_record + sidecar.record_offsets.len() - 1) as u64,
        ];
        let (mut drive, _) = damaged_image_drive(&image, faults);
        let walked = scan_reconstruct_filemark_map_with_report(
            &mut DriveHandleRawSource::new(&mut drive),
            &written.inputs.tape_uuid,
            BLOCK,
        )
        .unwrap();
        assert_eq!(walked.map, written.map);
        assert_eq!(walked.map.entries()[3].first_parity_data_ordinal, Some(4));
        assert_eq!(walked.map.entries()[4].kind, TapeFileKind::ParitySidecar);
    }
}
