//! Catalog method forwarding. Each call owns its lock locally; streaming visitors
//! are available only on independent readers.
use super::*;
use crate::audit::AuditRecord;
use crate::config::TapePoolRuleConfig;
use crate::index::*;
use crate::tape_log::authority::AuthoritySnapshot;
use crate::{DriveKey, LabelProvenance, MediaObservation};
use remanence_parity::authority::FilePublication;
use remanence_parity::authority::StructuralState;
use remanence_parity::{ParityScheme, TapeFileEntry};
use std::ops::ControlFlow;
use uuid::Uuid;

/// A readiness refusal from the typed runtime, as the state error callers match on. The runtime
/// already spells its refusals as `JournalReplayFailed`, so one is passed through; wrapping it
/// again printed "journal replay failed" twice before the cause.
fn readiness_error(error: crate::library_runtime::RuntimeError) -> StateError {
    match error {
        crate::library_runtime::RuntimeError::State(inner @ StateError::JournalReplayFailed(_)) => {
            inner
        }
        other => StateError::JournalReplayFailed(other.to_string()),
    }
}

impl CatalogIndex {
    /// Capture both projection watermarks and their source-byte bindings before rebuild.
    pub(crate) fn tape_projection_marks(
        &self,
    ) -> Result<
        std::collections::BTreeMap<[u8; 16], crate::library_runtime::native_floor::MarkEvidence>,
        StateError,
    > {
        self.with_core(|core| core.tape_projection_marks())
    }
    /// SQLite `PRAGMA user_version` after migration.
    pub fn schema_version(&self) -> Result<u32, StateError> {
        self.with_core(|core| core.schema_version())
    }
    /// Return whether a projection table exists.
    pub fn table_exists(&self, table_name: &str) -> Result<bool, StateError> {
        self.with_core(|core| core.table_exists(table_name))
    }
    /// Start or refresh a durable media-readiness operation.
    pub fn record_media_readiness_operation(
        &mut self,
        input: MediaReadinessOperationInput,
    ) -> Result<MediaReadinessOperationRecord, StateError> {
        if let Some(runtime) = self.library_runtime() {
            let operation = runtime
                .begin_owned_readiness(&input)
                .map_err(readiness_error)?;
            if let Some(observer) = self.readiness_observer.as_deref() {
                observer.changed();
            }
            return self.media_readiness_operation(operation)?.ok_or_else(|| {
                StateError::IndexCorrupt("typed readiness projection missing".into())
            });
        }
        #[cfg(any(test, feature = "fixtures"))]
        {
            self.with_core(|core| {
                core.record_media_readiness_operation(self.readiness_observer.as_deref(), input)
            })
        }
        #[cfg(not(any(test, feature = "fixtures")))]
        Err(StateError::ConfigInvalid(
            "readiness requires shared LibraryAuthority".into(),
        ))
    }
    /// Begin typed readiness and return its writer-minted identity. The caller
    /// must carry the returned id into its registry, LOAD context and polls.
    pub fn begin_typed_media_readiness_operation(
        &mut self,
        input: MediaReadinessOperationInput,
    ) -> Result<MediaReadinessOperationRecord, StateError> {
        let runtime = self.library_runtime().ok_or_else(|| {
            StateError::ConfigInvalid(
                "typed readiness requires the shared authority runtime".into(),
            )
        })?;
        let operation = runtime.begin_readiness(&input).map_err(readiness_error)?;
        if let Some(observer) = self.readiness_observer.as_deref() {
            observer.changed();
        }
        self.media_readiness_operation(operation)?
            .ok_or_else(|| StateError::IndexCorrupt("typed readiness projection missing".into()))
    }
    /// Record a durable transition for an existing media-readiness operation.
    pub fn record_media_readiness_transition(
        &mut self,
        input: MediaReadinessTransitionInput,
    ) -> Result<MediaReadinessOperationRecord, StateError> {
        if let Some(runtime) = self.library_runtime() {
            runtime
                .transition_readiness(&input)
                .map_err(readiness_error)?;
            if let Some(observer) = self.readiness_observer.as_deref() {
                observer.changed();
            }
            return self
                .media_readiness_operation(input.operation_id)?
                .ok_or_else(|| {
                    StateError::IndexCorrupt("typed readiness projection missing".into())
                });
        }
        #[cfg(any(test, feature = "fixtures"))]
        {
            self.with_core(|core| {
                core.record_media_readiness_transition(self.readiness_observer.as_deref(), input)
            })
        }
        #[cfg(not(any(test, feature = "fixtures")))]
        Err(StateError::ConfigInvalid(
            "readiness requires shared LibraryAuthority".into(),
        ))
    }
    /// Count the SSC LOAD attempts recorded for a media-readiness operation
    /// since its most recent `ready` transition.
    ///
    /// Production counts authenticated MotionIntent admissions since the latest
    /// Ready observation. Historical fixtures count their pre-ready rows. Readiness waits use it to bound the LOADs one
    /// operation sends, so a resumed or restarted wait does not repeat one.
    pub fn media_readiness_load_attempts_since_ready(
        &self,
        operation_id: Uuid,
    ) -> Result<u32, StateError> {
        if let Some(runtime) = self.library_runtime() {
            return runtime
                .readiness_load_attempts(*operation_id.as_bytes())
                .map_err(readiness_error);
        }
        #[cfg(any(test, feature = "fixtures"))]
        {
            self.with_core(|core| core.media_readiness_load_attempts_since_ready(operation_id))
        }
        #[cfg(not(any(test, feature = "fixtures")))]
        Err(StateError::ConfigInvalid(
            "readiness budget requires shared LibraryAuthority".into(),
        ))
    }
    /// Fetch a durable media-readiness operation by id.
    pub fn media_readiness_operation(
        &self,
        operation_id: Uuid,
    ) -> Result<Option<MediaReadinessOperationRecord>, StateError> {
        self.with_core(|core| core.media_readiness_operation(operation_id))
    }
    /// List active media-readiness fences, optionally scoped to one library.
    pub fn list_active_media_readiness_operations(
        &self,
        library_serial: Option<&str>,
    ) -> Result<Vec<MediaReadinessOperationRecord>, StateError> {
        self.with_core(|core| core.list_active_media_readiness_operations(library_serial))
    }
    /// Return active media-readiness fences that block a new operation.
    ///
    /// `library_robotics` should be true for operations that may issue
    /// changer moves or otherwise depend on the selected logical-library
    /// snapshot remaining settled. Specific drive/tape operations should also
    /// pass the resolved drive element and/or barcode.
    pub fn media_readiness_admission_conflicts<'a>(
        &self,
        target: impl Into<MediaReadinessAdmissionTarget<'a>>,
        drive_element: Option<u16>,
        barcode: Option<&str>,
        library_robotics: bool,
    ) -> Result<Vec<MediaReadinessOperationRecord>, StateError> {
        self.with_core(|core| {
            core.media_readiness_admission_conflicts(
                target,
                drive_element,
                barcode,
                library_robotics,
            )
        })
    }
    /// Persist a durable tape-I/O fence. Existing active fences for the same
    /// tape/barcode remain active; callers can present any one quarantine id
    /// to the operator.
    pub fn record_tape_io_fence(
        &mut self,
        input: TapeIoFenceInput,
    ) -> Result<TapeIoFenceRecord, StateError> {
        self.with_core(|core| core.record_tape_io_fence(input))
    }
    /// Record tape and endpoint fencing in the same durable insert.
    pub fn record_tape_io_fence_at_endpoint(
        &mut self,
        input: TapeIoFenceInput,
        endpoint: Option<&DriveKey>,
    ) -> Result<TapeIoFenceRecord, StateError> {
        self.with_core(|core| core.record_tape_io_fence_at_endpoint(input, endpoint))
    }
    /// Store (or replace) a volume's cached wrap map — the evicted
    /// projection built by the load harvest. The raw descriptors are
    /// stored exactly as harvested; nothing here validates them,
    /// because the harvest layer already ran the descriptors through
    /// `WrapMap::from_descriptors` before recording the calibrated
    /// transition this row is stamped with.
    pub fn upsert_wrap_map(&mut self, record: &WrapMapCacheRecord) -> Result<(), StateError> {
        self.with_core(|core| core.upsert_wrap_map(record))
    }
    /// Fetch a volume's cached wrap map, if one is present. Presence
    /// is not validity: the caller must check the durable
    /// calibration-control row before serving.
    pub fn get_wrap_map(
        &self,
        tape_uuid: &[u8; 16],
    ) -> Result<Option<WrapMapCacheRecord>, StateError> {
        self.with_core(|core| core.get_wrap_map(tape_uuid))
    }
    /// Evict one volume's cached wrap map. Returns whether a row
    /// existed. The durable calibration-control transition that
    /// accompanies eviction is the caller's responsibility — the
    /// projection cannot reach across erasure classes.
    pub fn delete_wrap_map(&mut self, tape_uuid: &[u8; 16]) -> Result<bool, StateError> {
        self.with_core(|core| core.delete_wrap_map(tape_uuid))
    }
    /// List active tape-I/O fences.
    pub fn list_active_tape_io_fences(&self) -> Result<Vec<TapeIoFenceRecord>, StateError> {
        self.with_core(|core| core.list_active_tape_io_fences())
    }
    /// Selection pre-filter projected from the library fold; absent identities are false.
    pub fn tape_contradicted(&self, tape: &[u8; 16]) -> Result<bool, StateError> {
        self.with_core(|core| core.tape_contradicted(tape))
    }
    /// Return active tape-I/O fences matching a tape UUID or barcode.
    pub fn tape_io_admission_conflicts(
        &self,
        tape_uuid: &[u8; 16],
        barcode: Option<&str>,
    ) -> Result<Vec<TapeIoFenceRecord>, StateError> {
        self.with_core(|core| core.tape_io_admission_conflicts(tape_uuid, barcode))
    }
    /// Check current tape identity, barcode, and endpoint fences without a cached view.
    pub fn tape_io_is_fenced(
        &self,
        tape_uuid: Option<&[u8; 16]>,
        barcode: Option<&str>,
        endpoint: Option<&DriveKey>,
    ) -> Result<bool, StateError> {
        self.with_core(|core| core.tape_io_is_fenced(tape_uuid, barcode, endpoint))
    }
    /// Release an active tape-I/O fence by quarantine id.
    pub fn release_tape_io_fence(
        &mut self,
        quarantine_id: &str,
        ack: &str,
    ) -> Result<Option<TapeIoFenceRecord>, StateError> {
        self.with_core(|core| core.release_tape_io_fence(quarantine_id, ack))
    }
    /// Project durable release evidence without changing its original time or acknowledgment.
    pub fn complete_tape_io_fence_release(
        &mut self,
        quarantine_id: &str,
        ack: &str,
        released_at: &str,
        forced: Option<bool>,
    ) -> Result<Option<TapeIoFenceRecord>, StateError> {
        self.with_core(|core| {
            core.complete_tape_io_fence_release(quarantine_id, ack, released_at, forced)
        })
    }
    /// Fetch one tape-I/O fence by quarantine id.
    pub fn tape_io_fence_by_quarantine_id(
        &self,
        quarantine_id: &str,
    ) -> Result<Option<TapeIoFenceRecord>, StateError> {
        self.with_core(|core| core.tape_io_fence_by_quarantine_id(quarantine_id))
    }
    /// Run SQLite quick_check and return the result text.
    pub fn quick_check(&self) -> Result<String, StateError> {
        self.with_core(|core| core.quick_check())
    }

    /// Index one fully replayed 3c committed state.
    pub fn index_structural_authority(
        &mut self,
        input: TapeJournalIndexInput,
        state: &StructuralState,
    ) -> Result<TapeJournalIndexReport, StateError> {
        self.with_core(|core| core.index_structural_authority(input, state))
    }
    /// Incrementally project one atomically committed 3c tape-file bundle.
    ///
    /// Live Layer 5 writes already know the object row and the 3c commit bundle
    /// at the commit boundary. This method updates the same rebuildable tables
    /// used by full tape-journal replay, without clearing earlier rows for the
    /// tape.
    #[cfg(any(test, feature = "fixtures"))]
    pub(crate) fn project_file_publication(
        &mut self,
        input: TapeJournalIndexInput,
        bundle: &FilePublication,
    ) -> Result<TapeJournalIndexReport, StateError> {
        self.with_core(|core| core.project_file_publication(input, bundle))
    }
    /// Replace the structural tape-file projection for one tape.
    ///
    /// Physical reconciliation can recover filemark geometry without recovering
    /// native object identities. This method intentionally touches only the
    /// `tape_files` projection and tape-level structural watermarks; object and
    /// object-instance projections remain owned by native object commit/journal
    /// replay paths.
    pub fn reconcile_tape_files_projection(
        &mut self,
        tape_uuid: [u8; 16],
        entries: &[TapeFileEntry],
        highest_protected_ordinal: u64,
        total_committed_ordinals: u64,
    ) -> Result<TapeJournalIndexReport, StateError> {
        self.with_core(|core| {
            core.reconcile_tape_files_projection(
                tape_uuid,
                entries,
                highest_protected_ordinal,
                total_committed_ordinals,
            )
        })
    }
    /// Readiness uses recorded media evidence whenever a catalog row exists.
    /// A label hint is permitted only before that row exists; an unknown recorded
    /// generation stays unknown rather than silently deriving a capacity class.
    pub fn readiness_media_observation(
        &self,
        label: Option<&str>,
    ) -> Result<Option<MediaObservation>, StateError> {
        self.with_core(|core| core.readiness_media_observation(label))
    }
    /// Register or refresh a blank/ready tape row without projecting objects.
    ///
    /// Provisioning owns only the `tapes` row. Pool membership is derived from
    /// the barcode via `[[tape_pool_rules]]` and projected onto `tapes.pool_id`
    /// by `reconcile_tape_pool_projection_from_rules`.
    pub fn provision_tape(&mut self, input: ProvisionTapeInput) -> Result<(), StateError> {
        self.with_core(|core| core.provision_tape(input))
    }
    /// Provision identity and its observed shell/media evidence atomically.
    /// A label hint cannot replace an existing measured/operator generation.
    pub fn provision_tape_with_media(
        &mut self,
        input: ProvisionTapeInput,
        provenance: LabelProvenance,
        observation: Option<MediaObservation>,
    ) -> Result<(), StateError> {
        self.with_core(|core| core.provision_tape_with_media(input, provenance, observation))
    }
    /// Reuse the provisioning transaction for a confirmed replacement of the
    /// observed BOT identity. Preview rolls back the exact same checks.
    pub fn provision_replacing_media(
        &mut self,
        input: ProvisionTapeInput,
        provenance: LabelProvenance,
        observation: Option<MediaObservation>,
        previous_uuid: Option<[u8; 16]>,
        preview: bool,
    ) -> Result<(), StateError> {
        self.with_core(|core| {
            core.provision_replacing_media(input, provenance, observation, previous_uuid, preview)
        })
    }
    /// Persist an explicit generation assertion for an already proven tape UUID.
    /// The caller owns the device/BOT proof; this updates no identity or capacity counters.
    pub fn set_tape_generation(
        &mut self,
        tape_uuid: [u8; 16],
        observation: MediaObservation,
    ) -> Result<(), StateError> {
        self.with_core(|core| core.set_tape_generation(tape_uuid, observation))
    }
    /// Mark a tape closed to future writes while preserving its catalog rows.
    /// The pool selector treats any non-`ready` tape as unwritable, so this
    /// state transition is enough to exclude a tape after eager sealing.
    pub fn seal_tape(&mut self, tape_uuid: [u8; 16]) -> Result<(), StateError> {
        self.with_core(|core| core.seal_tape(tape_uuid))
    }
    /// Reconcile configured cleaning-cartridge barcode prefixes after config load.
    pub fn reconcile_cleaning_prefixes(&mut self, prefixes: &[String]) -> Result<u64, StateError> {
        self.with_core(|core| core.reconcile_cleaning_prefixes(prefixes))
    }
    /// Register an inventory-identified cleaning cartridge by barcode.
    ///
    /// Cleaning media do not carry a Remanence BOT identity. Their stable
    /// catalog identity is therefore a namespaced UUID derived solely from
    /// the barcode. Existing data rows still pass through the kind-flip guard.
    pub fn ensure_cleaning_cartridge(&mut self, voltag: &str) -> Result<TapeRecord, StateError> {
        self.with_core(|core| core.ensure_cleaning_cartridge(voltag))
    }
    /// Record one drive inventory observation and assign or refresh its surrogate UUID.
    pub fn observe_drive(
        &mut self,
        input: DriveObservationInput,
    ) -> Result<DriveObservationOutcome, StateError> {
        if let Some(runtime) = self.library_runtime() {
            return runtime
                .observe_drives(vec![input])
                .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?
                .pop()
                .ok_or_else(|| {
                    StateError::IndexCorrupt("drive observation omitted result".into())
                });
        }
        self.with_core(|core| core.observe_drive(input))
    }
    /// Reconcile a whole inventory snapshot so duplicate serial claims seen in
    /// separate bays are kept as separate non-actionable drive rows.
    pub fn observe_drive_inventory_snapshot(
        &mut self,
        inputs: Vec<DriveObservationInput>,
    ) -> Result<Vec<DriveObservationOutcome>, StateError> {
        if let Some(runtime) = self.library_runtime() {
            return runtime
                .observe_drives(inputs)
                .map_err(|e| StateError::JournalReplayFailed(e.to_string()));
        }
        self.with_core(|core| core.observe_drive_inventory_snapshot(inputs))
    }
    /// List authoritative drive rows.
    pub fn list_drives(
        &self,
        include_foreign: bool,
        include_retired: bool,
    ) -> Result<Vec<DriveRecord>, StateError> {
        self.with_core(|core| core.list_drives(include_foreign, include_retired))
    }
    /// Fetch one drive by UUID bytes.
    pub fn get_drive_by_uuid(&self, drive_uuid: &[u8]) -> Result<Option<DriveRecord>, StateError> {
        self.with_core(|core| core.get_drive_by_uuid(drive_uuid))
    }
    /// Fetch one drive by UUID text or exact serial.
    pub fn get_drive_by_selector(&self, selector: &str) -> Result<Option<DriveRecord>, StateError> {
        self.with_core(|core| core.get_drive_by_selector(selector))
    }
    /// Fetch the active actionable drive currently observed at one library bay.
    pub fn get_actionable_drive_at(
        &self,
        library_serial: &str,
        element_address: i64,
    ) -> Result<Option<DriveRecord>, StateError> {
        self.with_core(|core| core.get_actionable_drive_at(library_serial, element_address))
    }
    /// List clean runs, including terminal rows when requested.
    pub fn list_clean_runs(
        &self,
        include_terminal: bool,
    ) -> Result<Vec<CleanRunRecord>, StateError> {
        self.with_core(|core| core.list_clean_runs(include_terminal))
    }
    /// Fetch the active clean run for one drive.
    pub fn get_active_clean_run_by_drive(
        &self,
        drive_uuid: &[u8],
    ) -> Result<Option<CleanRunRecord>, StateError> {
        self.with_core(|core| core.get_active_clean_run_by_drive(drive_uuid))
    }
    /// Fetch the active clean run for one cleaning cartridge.
    pub fn get_active_clean_run_by_cart(
        &self,
        cart_tape_uuid: &[u8],
    ) -> Result<Option<CleanRunRecord>, StateError> {
        self.with_core(|core| core.get_active_clean_run_by_cart(cart_tape_uuid))
    }
    /// Fence or unfence one drive in the durable catalog.
    pub fn set_drive_fenced(
        &mut self,
        drive_uuid: &[u8],
        fenced: bool,
    ) -> Result<Option<DriveRecord>, StateError> {
        self.with_core(|core| core.set_drive_fenced(drive_uuid, fenced))
    }
    /// Create or join the active clean run for one drive.
    pub fn begin_clean_run(
        &mut self,
        drive_uuid: &[u8],
        library_serial: &str,
        trigger: &str,
        detail: Option<&str>,
    ) -> Result<CleanRunRecord, StateError> {
        self.with_core(|core| core.begin_clean_run(drive_uuid, library_serial, trigger, detail))
    }
    /// Update the active clean run's selected cart and phase.
    pub fn select_clean_run_cart(
        &mut self,
        run_id: &str,
        cart_tape_uuid: &[u8],
        cart_home_slot: i64,
        detail: Option<&str>,
    ) -> Result<Option<CleanRunRecord>, StateError> {
        self.with_core(|core| {
            core.select_clean_run_cart(run_id, cart_tape_uuid, cart_home_slot, detail)
        })
    }
    /// Advance one clean run to a new phase.
    pub fn advance_clean_run(
        &mut self,
        run_id: &str,
        phase: &str,
        detail: Option<&str>,
    ) -> Result<Option<CleanRunRecord>, StateError> {
        self.with_core(|core| core.advance_clean_run(run_id, phase, detail))
    }
    /// Mark one clean run terminal with a failure-like phase.
    pub fn terminalize_clean_run(
        &mut self,
        run_id: &str,
        phase: &str,
        detail: Option<&str>,
    ) -> Result<Option<CleanRunRecord>, StateError> {
        self.with_core(|core| core.terminalize_clean_run(run_id, phase, detail))
    }
    /// Finish a clean run and apply verified credit.
    pub fn finalize_verified_clean_run(
        &mut self,
        run_id: &str,
        drive_uuid: &[u8],
        cart_tape_uuid: Option<&[u8]>,
        detail: Option<&str>,
    ) -> Result<Option<DriveRecord>, StateError> {
        self.with_core(|core| {
            core.finalize_verified_clean_run(run_id, drive_uuid, cart_tape_uuid, detail)
        })
    }
    /// Mark one clean run needing operator intervention.
    pub fn mark_clean_run_needs_operator(
        &mut self,
        run_id: &str,
        detail: Option<&str>,
    ) -> Result<Option<CleanRunRecord>, StateError> {
        self.with_core(|core| core.mark_clean_run_needs_operator(run_id, detail))
    }
    /// Mark one clean run failed.
    pub fn mark_clean_run_failed(
        &mut self,
        run_id: &str,
        detail: Option<&str>,
    ) -> Result<Option<CleanRunRecord>, StateError> {
        self.with_core(|core| core.mark_clean_run_failed(run_id, detail))
    }
    /// Reconcile active clean runs against one library snapshot.
    pub fn reconcile_clean_runs_against_library(
        &mut self,
        library: &remanence_library::Library,
    ) -> Result<u64, StateError> {
        self.with_core(|core| core.reconcile_clean_runs_against_library(library))
    }
    /// Return the active clean run for one library/drive combination.
    pub fn get_active_clean_run_for_drive(
        &self,
        drive_uuid: &[u8],
    ) -> Result<Option<CleanRunRecord>, StateError> {
        self.with_core(|core| core.get_active_clean_run_for_drive(drive_uuid))
    }
    /// Fetch one clean run by id.
    pub fn get_clean_run(&self, run_id: &str) -> Result<Option<CleanRunRecord>, StateError> {
        self.with_core(|core| core.get_clean_run(run_id))
    }
    /// Return observational drive history events.
    pub fn list_drive_events(
        &self,
        drive_uuid: &[u8],
    ) -> Result<Vec<DriveEventRecord>, StateError> {
        self.with_core(|core| core.list_drive_events(drive_uuid))
    }
    /// Return durable drive health snapshots.
    pub fn list_drive_health_snapshots(
        &self,
        drive_uuid: &[u8],
    ) -> Result<Vec<DriveHealthSnapshotRecord>, StateError> {
        self.with_core(|core| core.list_drive_health_snapshots(drive_uuid))
    }
    /// Return per-tape session/error rollups for one drive.
    pub fn drive_tape_correlation_rollups(
        &self,
        drive_uuid: &[u8],
    ) -> Result<Vec<DriveCorrelationRollupRecord>, StateError> {
        self.with_core(|core| core.drive_tape_correlation_rollups(drive_uuid))
    }
    /// Return per-drive session/error rollups for one tape.
    pub fn tape_drive_correlation_rollups(
        &self,
        tape_uuid: &[u8],
    ) -> Result<Vec<DriveCorrelationRollupRecord>, StateError> {
        self.with_core(|core| core.tape_drive_correlation_rollups(tape_uuid))
    }
    /// Insert a health snapshot idempotently by `(session_id, trigger)`.
    pub fn record_drive_health_snapshot(
        &mut self,
        input: DriveHealthSnapshotInput,
    ) -> Result<DriveHealthSnapshotRecord, StateError> {
        self.with_core(|core| core.record_drive_health_snapshot(input))
    }
    /// Persist a managed drive cleaning-due observation monotonically.
    pub fn observe_managed_drive_cleaning_due(
        &mut self,
        drive_uuid: &[u8],
        cleaning_due: &str,
    ) -> Result<Option<DriveRecord>, StateError> {
        self.with_core(|core| core.observe_managed_drive_cleaning_due(drive_uuid, cleaning_due))
    }
    /// Raise, refresh, or clear the advisory for opted-in foreign TapeAlert reads.
    pub fn observe_foreign_drive_tapealert_advisory(
        &mut self,
        drive_uuid: &[u8],
        tape_alert_flags: Option<&str>,
    ) -> Result<Option<AlarmRecord>, StateError> {
        self.with_core(|core| {
            core.observe_foreign_drive_tapealert_advisory(drive_uuid, tape_alert_flags)
        })
    }
    /// Upsert a standing alarm as open or refreshed.
    pub fn raise_alarm(
        &mut self,
        condition_key: &str,
        kind: &str,
        severity: &str,
        detail: Option<&str>,
    ) -> Result<AlarmRecord, StateError> {
        self.with_core(|core| core.raise_alarm(condition_key, kind, severity, detail))
    }
    /// Mark a standing alarm acknowledged.
    pub fn ack_alarm(
        &mut self,
        condition_key: &str,
        acked_by: &str,
    ) -> Result<Option<AlarmRecord>, StateError> {
        self.with_core(|core| core.ack_alarm(condition_key, acked_by))
    }
    /// Mark a standing alarm cleared.
    pub fn clear_alarm(&mut self, condition_key: &str) -> Result<Option<AlarmRecord>, StateError> {
        self.with_core(|core| core.clear_alarm(condition_key))
    }
    /// List standing alarms.
    pub fn list_alarms(&self, include_cleared: bool) -> Result<Vec<AlarmRecord>, StateError> {
        self.with_core(|core| core.list_alarms(include_cleared))
    }
    /// Fetch one alarm by condition key.
    pub fn get_alarm(&self, condition_key: &str) -> Result<Option<AlarmRecord>, StateError> {
        self.with_core(|core| core.get_alarm(condition_key))
    }
    /// Apply a partial operator annotation to one drive.
    pub fn annotate_drive(
        &mut self,
        input: DriveAnnotationInput,
    ) -> Result<Option<DriveRecord>, StateError> {
        self.with_core(|core| core.annotate_drive(input))
    }
    /// Retire one drive identity while preserving its history.
    pub fn retire_drive(
        &mut self,
        drive_uuid: &[u8],
        reason: &str,
    ) -> Result<Option<RetireDriveOutcome>, StateError> {
        self.with_core(|core| core.retire_drive(drive_uuid, reason))
    }
    /// Return a retired drive to service.
    ///
    /// Retirement is a catalog judgement, not a physical fact: a drive can be
    /// retired in error, or retired and then genuinely returned to the fleet.
    /// Reinstatement is the audited way back. It clears the retirement
    /// timestamp and reason so the row cannot later be read as both retired and
    /// active, and records a `reinstated` event carrying the operator's reason
    /// alongside the retirement it reverses.
    ///
    /// It does not restore `actionable` or clear `fenced`: those are separate
    /// judgements with their own gates, and a reinstated drive that is still
    /// fenced remains correctly unusable.
    pub fn reinstate_drive(
        &mut self,
        drive_uuid: &[u8],
        reason: &str,
    ) -> Result<Option<ReinstateDriveOutcome>, StateError> {
        self.with_core(|core| core.reinstate_drive(drive_uuid, reason))
    }
    /// Permanently retire one tape identity and mark its instances missing.
    ///
    /// `retired` is terminal: no code path leaves it, and `provision_tape`
    /// refuses to reuse the row even with `force`. The voltag is detached in
    /// the same transaction (the partial unique index `tapes_voltag_unique`
    /// makes detach-before-rebind mandatory); the released voltag is recorded
    /// in the returned outcome for the audit detail, not in a column.
    /// `pool_id` is kept as history — selection gates on state, so a retired
    /// row in a pool is harmless and the provenance is useful.
    ///
    /// Invariant: instance status is derived from tape state. A instance on a retired
    /// tape is `missing`, always; this method enforces it for the live
    /// catalog and rebuild re-derives it after journal replay, so no
    /// instance-level preservation machinery is needed.
    pub fn retire_tape(&mut self, input: RetireTapeInput) -> Result<RetireTapeOutcome, StateError> {
        self.with_core(|core| core.retire_tape(input))
    }
    /// Object ids that have instance rows but no `committed` instance anywhere.
    ///
    /// This is the degraded-objects hook for retire reporting and future
    /// self-heal: objects listed here exist in the catalog but cannot be
    /// read back from any tape.
    pub fn list_objects_with_no_committed_instances(&self) -> Result<Vec<String>, StateError> {
        self.with_core(|core| core.list_objects_with_no_committed_instances())
    }
    /// Project a native object commit and its 3c tape-file bundle atomically.
    ///
    /// Live Layer 5 writes produce both projections at one commit boundary.
    /// This method keeps the object/instance rows and tape-file bundle rows in one
    /// SQLite transaction so a later bundle projection error cannot leave an
    /// orphan committed object instance visible in the catalog.
    pub fn project_native_object_and_committed_tape_file_bundle(
        &mut self,
        object: NativeObjectProjectionInput,
        files: &[NativeObjectFileProjectionInput],
        instances: &[NativeObjectInstanceProjectionInput],
        tape_input: TapeJournalIndexInput,
        bundle: &FilePublication,
    ) -> Result<TapeJournalIndexReport, StateError> {
        self.with_core(|core| {
            core.project_native_object_and_committed_tape_file_bundle(
                object, files, instances, tape_input, bundle,
            )
        })
    }
    /// Project one live append commit as a strict prefix extension.
    ///
    /// This is the MTA-1 append path for live pool writes. Unlike the legacy
    /// bundle projection used for rebuild and older parity commits, this method
    /// rejects skipped tape-file numbers, overlapping tape-file rows, geometry
    /// changes, watermark regressions, and non-writable tape states before any
    /// object rows become visible.
    pub fn project_native_object_append_commit(
        &mut self,
        object: NativeObjectProjectionInput,
        files: &[NativeObjectFileProjectionInput],
        instances: &[NativeObjectInstanceProjectionInput],
        tape_input: TapeJournalIndexInput,
        bundle: &FilePublication,
    ) -> Result<TapeJournalIndexReport, StateError> {
        self.with_core(|core| {
            core.project_native_object_append_commit(object, files, instances, tape_input, bundle)
        })
    }
    /// Replay the imported authority pair and check its restrictions. It writes
    /// nothing: ruling 32-L1 removed the SQL clear of append recovery, because
    /// journal import refuses before any effect until the library log records it.
    /// An active quarantine or any terminal-finalization state remains a refusal.
    pub fn recover_imported_append_authority(
        &mut self,
        checkpoint: &crate::TapeAuthorityLease,
        parity: Option<&dyn remanence_parity::authority::StructuralAuthority>,
        endpoint: &DriveKey,
    ) -> Result<(), StateError> {
        self.with_core(|core| core.recover_imported_append_authority(checkpoint, parity, endpoint))
    }
    /// Mark a tape journal as pending because a live append session owns it.
    pub fn mark_tape_journal_ingestion_pending(
        &mut self,
        tape_uuid: [u8; 16],
        block_size: u32,
        scheme: &ParityScheme,
    ) -> Result<TapeJournalIndexReport, StateError> {
        self.with_core(|core| {
            core.mark_tape_journal_ingestion_pending(tape_uuid, block_size, scheme)
        })
    }
    /// Rebuild audit-derived operation, session, and idempotency projections.
    pub fn replay_audit_records(
        &mut self,
        records: &[AuditRecord],
    ) -> Result<AuditReplayReport, StateError> {
        self.with_core(|core| core.replay_audit_records(records))
    }
    /// Rebuild all Layer 4-owned projections from authoritative sources.
    pub fn rebuild_from_authoritative_sources(
        &mut self,
        audit_records: &[AuditRecord],
        tape_journals: &[RebuildTapeJournalInput],
    ) -> Result<RebuildReport, StateError> {
        self.with_core(|core| core.rebuild_from_authoritative_sources(audit_records, tape_journals))
    }
    /// Incrementally project one newly appended audit record.
    pub fn project_audit_record(&mut self, record: &AuditRecord) -> Result<(), StateError> {
        self.with_core(|core| core.project_audit_record(record))
    }
    /// Project one operator-defined tape pool from config or audit authority.
    pub fn upsert_tape_pool_projection(
        &mut self,
        input: TapePoolProjectionInput,
    ) -> Result<(), StateError> {
        self.with_core(|core| core.upsert_tape_pool_projection(input))
    }
    /// Reconcile config-authoritative tape pools and barcode-derived memberships.
    ///
    /// Pool definitions come from `[[tape_pools]]`; memberships are recomputed
    /// from current catalog tape voltags and `[[tape_pool_rules]]`. Tapes whose
    /// voltags match no rule are projected with no current pool.
    pub fn reconcile_tape_pool_projection_from_rules(
        &mut self,
        pools: &[TapePoolProjectionInput],
        rules: &[TapePoolRuleConfig],
    ) -> Result<(), StateError> {
        self.with_core(|core| core.reconcile_tape_pool_projection_from_rules(pools, rules))
    }
    /// Assign an empty tape to a pool under the guarded assignment transaction.
    pub fn project_tape_pool_membership(
        &mut self,
        tape_uuid: [u8; 16],
        pool_id: &str,
    ) -> Result<(), StateError> {
        self.with_core(|core| core.project_tape_pool_membership(tape_uuid, pool_id))
    }
    /// Clear a tape's pool membership under its current assignment generation.
    pub fn clear_tape_pool_membership(&mut self, tape_uuid: [u8; 16]) -> Result<(), StateError> {
        self.with_core(|core| core.clear_tape_pool_membership(tape_uuid))
    }
    /// Read the pool assignment and generation as one conditional-update snapshot.
    pub fn get_tape_assignment_snapshot(
        &self,
        tape_uuid: &[u8; 16],
    ) -> Result<Option<TapeAssignmentSnapshot>, StateError> {
        self.with_core(|core| core.get_tape_assignment_snapshot(tape_uuid))
    }
    /// Conditionally assign or clear a pool using the observed generation.
    ///
    /// An effective change increments the generation exactly once. Repeating
    /// the same value is idempotent and retains the current generation.
    pub fn compare_and_set_tape_pool_assignment(
        &mut self,
        tape_uuid: [u8; 16],
        expected_generation: u64,
        pool_id: Option<&str>,
    ) -> Result<TapeAssignmentSnapshot, StateError> {
        self.with_core(|core| {
            core.compare_and_set_tape_pool_assignment(tape_uuid, expected_generation, pool_id)
        })
    }
    /// Assign or unassign a pool with the authenticated/local actor recorded in
    /// the same transaction as the guarded projection update.
    pub fn compare_and_set_tape_pool_assignment_as(
        &mut self,
        tape_uuid: [u8; 16],
        expected_generation: u64,
        pool_id: Option<&str>,
        actor: &str,
    ) -> Result<TapeAssignmentSnapshot, StateError> {
        self.with_core(|core| {
            core.compare_and_set_tape_pool_assignment_as(
                tape_uuid,
                expected_generation,
                pool_id,
                actor,
            )
        })
    }
    /// List configured tape pools.
    #[cfg(test)]
    pub(crate) fn compare_and_set_tape_pool_assignment_with_audit(
        &mut self,
        tape_uuid: [u8; 16],
        expected_generation: u64,
        pool_id: Option<&str>,
        actor: &str,
        before_commit: impl FnOnce(&TapeAssignmentSnapshot) -> Result<(), StateError>,
    ) -> Result<TapeAssignmentSnapshot, StateError> {
        self.with_core(|core| {
            core.compare_and_set_tape_pool_assignment_with_audit(
                tape_uuid,
                expected_generation,
                pool_id,
                actor,
                before_commit,
            )
        })
    }
    /// List configured tape pools.
    pub fn list_tape_pools(&self) -> Result<Vec<TapePoolRecord>, StateError> {
        self.with_core(|core| core.list_tape_pools())
    }
    /// Fetch one configured tape pool.
    pub fn get_tape_pool(&self, pool_id: &str) -> Result<Option<TapePoolRecord>, StateError> {
        self.with_core(|core| core.get_tape_pool(pool_id))
    }
    /// Return the current pool assignment for one tape, if assigned.
    pub fn get_tape_pool_membership(
        &self,
        tape_uuid: &[u8; 16],
    ) -> Result<Option<String>, StateError> {
        self.with_core(|core| core.get_tape_pool_membership(tape_uuid))
    }
    /// Project one monotonic terminal-finalization state for a known tape.
    pub fn project_terminal_finalization(
        &mut self,
        input: TerminalFinalizationProjectionInput,
    ) -> Result<TapeRecord, StateError> {
        self.with_core(|core| core.project_terminal_finalization(input))
    }
    /// Atomically accept one guarded operator close-out.
    ///
    /// The immediate transaction is intentionally held while `publish_intent`
    /// fsyncs the exact checkpoint companion. Assignment writers therefore
    /// either commit before the guarded reread in this transaction or observe
    /// the committed `Finalizing` fence afterwards. The idempotency scope and
    /// `BeforeReplicaA` projection commit in that same transaction.
    pub fn accept_manual_terminal_finalization_with(
        &mut self,
        input: ManualTerminalFinalizationAcceptanceInput,
        publish_intent: impl FnOnce() -> Result<(), StateError>,
    ) -> Result<TapeRecord, StateError> {
        self.with_core(|core| core.accept_manual_terminal_finalization_with(input, publish_intent))
    }
    /// Return a tape's durable terminal-finalization projection.
    pub fn terminal_finalization(
        &self,
        tape_uuid: &[u8; 16],
    ) -> Result<Option<TerminalFinalizationProjection>, StateError> {
        self.with_core(|core| core.terminal_finalization(tape_uuid))
    }
    /// Project a native object commit and its concrete tape instances into Layer 4.
    pub fn upsert_native_object_projection(
        &mut self,
        object: NativeObjectProjectionInput,
        instances: &[NativeObjectInstanceProjectionInput],
    ) -> Result<(), StateError> {
        self.with_core(|core| core.upsert_native_object_projection(object, instances))
    }
    /// List source-neutral catalog units for operator/discovery surfaces.
    pub fn list_catalog_units(
        &self,
        filter: CatalogUnitFilter,
    ) -> Result<Vec<CatalogUnitRecord>, StateError> {
        self.with_core(|core| core.list_catalog_units(filter))
    }
    /// List known tapes from the rebuildable projection.
    pub fn list_tapes(
        &self,
        pool_id: Option<&str>,
        kind_filter: TapeKindFilter,
    ) -> Result<Vec<TapeRecord>, StateError> {
        self.with_core(|core| core.list_tapes(pool_id, kind_filter))
    }
    /// Update a tape kind with the kind-flip guard.
    pub fn set_tape_kind(
        &mut self,
        tape_uuid: &[u8],
        kind: &str,
    ) -> Result<Option<TapeRecord>, StateError> {
        self.with_core(|core| core.set_tape_kind(tape_uuid, kind))
    }
    /// Update a cleaning cartridge lifecycle state.
    pub fn set_tape_cleaning_state(
        &mut self,
        tape_uuid: &[u8],
        state: &str,
    ) -> Result<Option<TapeRecord>, StateError> {
        self.with_core(|core| core.set_tape_cleaning_state(tape_uuid, state))
    }
    /// Fetch one tape's cleaning state directly.
    pub fn get_tape_cleaning_state(
        &self,
        tape_uuid: &[u8],
    ) -> Result<Option<Option<String>>, StateError> {
        self.with_core(|core| core.get_tape_cleaning_state(tape_uuid))
    }
    /// Fetch one known tape by UUID.
    pub fn get_tape(&self, tape_uuid: &[u8]) -> Result<Option<TapeRecord>, StateError> {
        self.with_core(|core| core.get_tape(tape_uuid))
    }
    /// Fetch one known tape by operator-facing barcode / volume tag.
    pub fn get_tape_by_voltag(&self, voltag: &str) -> Result<Option<TapeRecord>, StateError> {
        self.with_core(|core| core.get_tape_by_voltag(voltag))
    }
    /// Current pools on committed instances for one tape (legacy method name).
    pub fn committed_instance_pool_snapshots(
        &self,
        tape_uuid: &[u8],
    ) -> Result<Vec<Option<String>>, StateError> {
        self.with_core(|core| core.committed_instance_pool_snapshots(tape_uuid))
    }
    /// List committed tape files for one tape.
    pub fn list_tape_files(&self, tape_uuid: &[u8]) -> Result<Vec<TapeFileRecord>, StateError> {
        self.with_core(|core| core.list_tape_files(tape_uuid))
    }
    /// Fetch one source-neutral catalog unit by stable id.
    pub fn get_catalog_unit(&self, unit_id: &str) -> Result<Option<CatalogUnitRecord>, StateError> {
        self.with_core(|core| core.get_catalog_unit(unit_id))
    }
    /// List native objects for the Layer 5 object hot path.
    pub fn list_native_objects(&self) -> Result<Vec<NativeObjectRecord>, StateError> {
        self.with_core(|core| core.list_native_objects())
    }
    /// Fetch a native object and its instances by object id.
    pub fn get_native_object(
        &self,
        object_id: &str,
    ) -> Result<Option<NativeObjectRecord>, StateError> {
        self.with_core(|core| core.get_native_object(object_id))
    }
    /// Fetch one native object member-file row by object id and file id.
    pub fn get_native_object_file(
        &self,
        object_id: &str,
        file_id: &str,
    ) -> Result<Option<NativeObjectFileRecord>, StateError> {
        self.with_core(|core| core.get_native_object_file(object_id, file_id))
    }
    /// List native object member-file rows.
    pub fn list_native_object_files(
        &self,
        object_id: &str,
    ) -> Result<Vec<NativeObjectFileRecord>, StateError> {
        self.with_core(|core| core.list_native_object_files(object_id))
    }
    /// Newest committed versions in the instance's tape pool, independent of tape assignment.
    pub fn list_committed_versions_by_path(
        &self,
        pool_id: &str,
        archive_path: &str,
    ) -> Result<Vec<VersionRow>, StateError> {
        self.with_core(|core| core.list_committed_versions_by_path(pool_id, archive_path))
    }
    /// Newest versions with a committed instance on the specified cartridge.
    pub fn list_committed_versions_by_path_on_tape(
        &self,
        archive_path: &str,
        tape_uuid: &[u8; 16],
    ) -> Result<Vec<VersionRow>, StateError> {
        self.with_core(|core| core.list_committed_versions_by_path_on_tape(archive_path, tape_uuid))
    }
    /// All committed locations for a path, used to explain a wrong-tape lookup.
    pub fn list_committed_versions_by_path_any_pool(
        &self,
        archive_path: &str,
    ) -> Result<Vec<VersionRow>, StateError> {
        self.with_core(|core| core.list_committed_versions_by_path_any_pool(archive_path))
    }
    /// Read committed path inventories, grouped by tape pool, with deterministic newest versions.
    pub fn list_committed_paths(
        &self,
        tape: Option<&str>,
        prefix: Option<&str>,
    ) -> Result<Vec<serde_json::Value>, StateError> {
        self.with_core(|core| core.list_committed_paths(tape, prefix))
    }
    /// Fetch a native object and its instances by content hash.
    pub fn get_native_object_by_content_hash(
        &self,
        content_hash: &[u8],
    ) -> Result<Option<NativeObjectRecord>, StateError> {
        self.with_core(|core| core.get_native_object_by_content_hash(content_hash))
    }
    /// Resolve an unscoped caller id only when one committed pool contains it.
    pub fn get_native_object_by_unique_caller_object_id(
        &self,
        caller_object_id: &str,
    ) -> Result<Option<NativeObjectRecord>, StateError> {
        self.with_core(|core| core.get_native_object_by_unique_caller_object_id(caller_object_id))
    }
    /// Fetch a native object by caller id only when it has a committed instance in
    /// the requested pool.
    pub fn get_native_object_by_pool_and_caller_object_id(
        &self,
        pool_id: &str,
        caller_object_id: &str,
    ) -> Result<Option<NativeObjectRecord>, StateError> {
        self.with_core(|core| {
            core.get_native_object_by_pool_and_caller_object_id(pool_id, caller_object_id)
        })
    }
    /// Return concrete tape instances for a native object.
    pub fn find_native_object_instances(
        &self,
        object_id: &str,
    ) -> Result<Vec<NativeObjectInstanceRecord>, StateError> {
        self.with_core(|core| core.find_native_object_instances(object_id))
    }
    /// Fetch one projected operation by UUID text.
    pub fn get_operation(&self, operation_id: &str) -> Result<Option<OperationRecord>, StateError> {
        self.with_core(|core| core.get_operation(operation_id))
    }
    /// List projected operations in most-recent update order.
    pub fn list_operations(&self) -> Result<Vec<OperationRecord>, StateError> {
        self.with_core(|core| core.list_operations())
    }
    /// Return operations that are not terminal after audit replay.
    pub fn non_terminal_operations(&self) -> Result<Vec<RestartOperation>, StateError> {
        self.with_core(|core| core.non_terminal_operations())
    }
    /// Return sessions that are not terminal after audit replay.
    pub fn non_terminal_sessions(&self) -> Result<Vec<RestartSession>, StateError> {
        self.with_core(|core| core.non_terminal_sessions())
    }
    /// Return the projected operation state for typed callers and tests.
    pub fn operation_state(&self, operation_id: Uuid) -> Result<Option<String>, StateError> {
        self.with_core(|core| core.operation_state(operation_id))
    }
    /// Return the projected session state for typed callers and tests.
    pub fn session_state(&self, session_id: Uuid) -> Result<Option<String>, StateError> {
        self.with_core(|core| core.session_state(session_id))
    }
    /// Reserve one actor/operation-kind/idempotency-key scope.
    ///
    /// An identical request returns the operation originally registered for
    /// the scope. Reuse with a different request fingerprint is a typed
    /// conflict and never rewrites the original operation.
    pub fn register_idempotency_request(
        &mut self,
        actor_fingerprint: &str,
        operation_kind: &str,
        idempotency_key: Uuid,
        request_fingerprint: [u8; 32],
        operation_id: Uuid,
        updated_at_utc: Option<&str>,
    ) -> Result<IdempotencyRegistration, StateError> {
        self.with_core(|core| {
            core.register_idempotency_request(
                actor_fingerprint,
                operation_kind,
                idempotency_key,
                request_fingerprint,
                operation_id,
                updated_at_utc,
            )
        })
    }
    /// Read the exact durable idempotency binding before creating a new plan.
    pub fn idempotency_scope_record(
        &self,
        actor_fingerprint: &str,
        operation_kind: &str,
        idempotency_key: Uuid,
    ) -> Result<Option<IdempotencyScopeRecord>, StateError> {
        self.with_core(|core| {
            core.idempotency_scope_record(actor_fingerprint, operation_kind, idempotency_key)
        })
    }
    /// Return terminal idempotency state for the complete primary scope.
    pub fn idempotency_terminal_state_scoped(
        &self,
        actor_fingerprint: &str,
        operation_kind: &str,
        idempotency_key: Uuid,
    ) -> Result<Option<String>, StateError> {
        self.with_core(|core| {
            core.idempotency_terminal_state_scoped(
                actor_fingerprint,
                operation_kind,
                idempotency_key,
            )
        })
    }
    /// Return terminal idempotency state when actor/key resolves uniquely.
    ///
    /// New state-changing callers should use
    /// [`Self::idempotency_terminal_state_scoped`]. This compatibility lookup
    /// fails closed if the same actor/key exists under multiple operation kinds.
    pub fn idempotency_terminal_state(
        &self,
        actor_fingerprint: &str,
        idempotency_key: Uuid,
    ) -> Result<Option<String>, StateError> {
        self.with_core(|core| core.idempotency_terminal_state(actor_fingerprint, idempotency_key))
    }
    /// Catalog accelerator only; the log remains authority and must be validated.
    pub fn projected_through_sequence(&self, uuid: [u8; 16]) -> Result<u64, StateError> {
        self.with_core(|core| core.projected_through_sequence(uuid))
    }
    /// Tape identities with acknowledged projection marks, including missing logs.
    pub fn projected_tape_uuids(&self) -> Result<Vec<[u8; 16]>, StateError> {
        self.with_core(|core| core.projected_tape_uuids())
    }
    /// Project all new closed groups from one immutable, validated snapshot.
    pub fn project_tape_log(&mut self, snapshot: &AuthoritySnapshot) -> Result<(), StateError> {
        if let Some(library) = self.library.clone() {
            library
                .project_native(self, snapshot, super::apply::ApplyPath::Live)
                .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?;
            return Ok(());
        }
        self.with_core(|core| core.project_tape_log(snapshot))?;
        Ok(())
    }
    /// Projection-only rebuild: this entry point has no filesystem reset access.
    pub fn rebuild_tape_log_projection(
        &mut self,
        audit: &[AuditRecord],
        snapshots: &[AuthoritySnapshot],
    ) -> Result<RebuildReport, StateError> {
        self.with_core(|core| core.rebuild_tape_log_projection(audit, snapshots))
    }
    /// Full typed replacement retains its single writer lease and rollback scope.
    pub(crate) fn rebuild_typed_sources(
        &mut self,
        replay: impl FnOnce(
            &mut dyn FnMut(
                crate::library_log::AppliedLibraryRecord,
            ) -> Result<(), crate::library_log::store::StoreError>,
        ) -> Result<
            crate::library_log::store::Replay,
            crate::library_log::store::StoreError,
        >,
        snapshots: &[AuthoritySnapshot],
    ) -> Result<RebuildReport, super::apply::ApplyError> {
        self.with_core(|core| Ok(core.rebuild_typed_sources(replay, snapshots)))?
    }
    /// Typed replay counts for the compatibility report type.
    pub(crate) fn typed_replay_report(&self) -> Result<AuditReplayReport, StateError> {
        self.with_core(|core| core.typed_replay_report())
    }
    /// A repair cannot erase accepted terminal obligations recorded in SQLite,
    /// including surviving idempotency/operation rows. Forensic recovery is a
    /// separate operator decision outside the step-2 repair/supersede paths.
    // Only the L2 repair path (ruling 32) calls this in production; test-only until then.
    #[cfg(test)]
    pub(crate) fn validate_repair_prefix(
        &self,
        snapshot: &AuthoritySnapshot,
    ) -> Result<(), StateError> {
        self.with_core(|core| core.validate_repair_prefix(snapshot))
    }
    /// Replace only a repaired tape's derived rows; authority was archived and
    /// remains physically fenced until the operator proves its append tail.
    ///
    /// This explicit rebuild is the one projection that may lower the written
    /// extent: it re-derives every row of the tape from the retained prefix
    /// alone. It does not change the tape's state. Ruling 32-L1 refuses tail
    /// repair before any effect, so no production path calls it until the library
    /// log records repairs. A full catalog rebuild likewise has no prior extent
    /// and derives it from each log.
    pub fn rebuild_repaired_tape_projection(
        &mut self,
        snapshot: &AuthoritySnapshot,
    ) -> Result<(), StateError> {
        self.with_core(|core| core.rebuild_repaired_tape_projection(snapshot))
    }
    /// Publish a semantic fixture to a native log, then reconstruct its catalog.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn project_checkpoint_record(
        &mut self,
        record: &crate::CommitFixture,
    ) -> Result<(), StateError> {
        self.with_core(|core| core.project_checkpoint_record(record, self.path()))
    }
}

// Streaming callbacks hold only the reader's independent connection lock.
impl CatalogReader {
    /// Visit source-neutral catalog units without materializing the full query.
    pub fn for_each_catalog_unit<F>(
        &self,
        filter: CatalogUnitFilter,
        visit: F,
    ) -> Result<(), StateError>
    where
        F: FnMut(CatalogUnitRecord) -> ControlFlow<()>,
    {
        self.with_core(|core| core.for_each_catalog_unit(filter, visit))
    }
    /// Visit native objects in catalog order without materializing the table.
    pub fn for_each_native_object<F>(&self, visit: F) -> Result<(), StateError>
    where
        F: FnMut(NativeObjectRecord) -> ControlFlow<()>,
    {
        self.with_core(|core| core.for_each_native_object(visit))
    }
}
