/- Specification theorems for the tape-init decision extraction (SPEC.md T1-T5).

   Targets the Aeneas-generated definitions in `TapeInit.Funs`. These theorems
   certify the pure decision ordering for `decide_tape_init`: pool-conflict
   dominance, fail-closed BOT handling, blank-tape handling, the clean
   Remanence-bootstrap no-op path, and ordered refusal/anomaly paths. The Rust
   `drift_guard` test ties this proof-facing model back to production
   `crates/remanence-api/src/tape_init.rs`. -/
import TapeInit.Funs

open Aeneas Aeneas.Std Result

namespace TapeInit

/- Formal-proof scope:
   the theorems below certify the extracted pure tape-init safety gauntlet.
   They do not prove BOT reading, catalog projection, bootstrap writing, string
   payload text, or hardware orchestration. If the mirrored Rust logic changes,
   update the extraction and rerun `lake build`; the Rust drift_guard is
   intended to catch stale proofs. -/

def cleanRow (uuid : Std.U64) (geometry : TapeInitGeometry) : CatalogTapeInitRow :=
  { uuid := uuid
    geometry := geometry
    catalog_unwritten := true
    barcode_relation := CatalogBarcodeRelation.Matches
    disposition := CatalogRowDisposition.Active }

def relabeledCleanRow (uuid : Std.U64) (geometry : TapeInitGeometry) : CatalogTapeInitRow :=
  { cleanRow uuid geometry with
    barcode_relation := CatalogBarcodeRelation.DiffersWithRecordedRelabel }

def unrecordedRelabelRow (uuid : Std.U64) (geometry : TapeInitGeometry) : CatalogTapeInitRow :=
  { cleanRow uuid geometry with
    barcode_relation := CatalogBarcodeRelation.DiffersWithoutRelabel }

def retiredRow (uuid : Std.U64) (geometry : TapeInitGeometry) : CatalogTapeInitRow :=
  { uuid := uuid
    geometry := geometry
    catalog_unwritten := false
    barcode_relation := CatalogBarcodeRelation.DiffersWithoutRelabel
    disposition := CatalogRowDisposition.Retired }

def writtenRow (uuid : Std.U64) (geometry : TapeInitGeometry) : CatalogTapeInitRow :=
  { cleanRow uuid geometry with
    catalog_unwritten := false }

theorem geometry_matches_same (geometry : TapeInitGeometry) :
    geometry_matches geometry geometry = ok true := by
  simp [geometry_matches]

theorem geometry_block_size_mismatch_false (left right : TapeInitGeometry)
    (h : left.block_size_bytes ≠ right.block_size_bytes) :
    geometry_matches left right = ok false := by
  simp [geometry_matches, h]

theorem geometry_parity_mismatch_false (left right : TapeInitGeometry)
    (hBlocks : left.block_size_bytes = right.block_size_bytes)
    (hParity : left.parity_mode ≠ right.parity_mode) :
    geometry_matches left right = ok false := by
  simp [geometry_matches, hBlocks, hParity]

theorem committed_foreign_pool_conflict_precedes_all
    (bot : BotClassification) (catalog_row : Option CatalogTapeInitRow)
    (barcode_state : BarcodeLifecycleState) (physical_data_past_bootstrap : Bool) :
    decide_tape_init bot catalog_row barcode_state physical_data_past_bootstrap
      CommittedCopyState.ForeignPool =
      ok (InitDecision.Anomaly TapeInitError.TapePoolAssignmentConflict) := by
  cases bot <;> rfl

theorem committed_unknown_pool_conflict_precedes_all
    (bot : BotClassification) (catalog_row : Option CatalogTapeInitRow)
    (barcode_state : BarcodeLifecycleState) (physical_data_past_bootstrap : Bool) :
    decide_tape_init bot catalog_row barcode_state physical_data_past_bootstrap
      CommittedCopyState.UnknownPool =
      ok (InitDecision.Anomaly TapeInitError.TapePoolAssignmentConflict) := by
  cases bot <;> rfl

theorem read_error_refuses_fail_closed
    (catalog_row : Option CatalogTapeInitRow) (barcode_state : BarcodeLifecycleState)
    (physical_data_past_bootstrap : Bool) :
    decide_tape_init BotClassification.ReadError catalog_row barcode_state
      physical_data_past_bootstrap CommittedCopyState.None =
      ok (InitDecision.RefuseClobber TapeInitError.BotReadError) := by
  rfl

theorem foreign_format_refuses
    (catalog_row : Option CatalogTapeInitRow) (barcode_state : BarcodeLifecycleState)
    (physical_data_past_bootstrap : Bool) :
    decide_tape_init BotClassification.ForeignFormat catalog_row barcode_state
      physical_data_past_bootstrap CommittedCopyState.None =
      ok (InitDecision.RefuseClobber TapeInitError.ForeignFormat) := by
  rfl

theorem unrecognized_data_refuses
    (catalog_row : Option CatalogTapeInitRow) (barcode_state : BarcodeLifecycleState)
    (physical_data_past_bootstrap : Bool) :
    decide_tape_init BotClassification.UnrecognizedData catalog_row barcode_state
      physical_data_past_bootstrap CommittedCopyState.None =
      ok (InitDecision.RefuseClobber TapeInitError.UnrecognizedData) := by
  rfl

theorem blank_available_no_copies_is_fresh :
    decide_tape_init BotClassification.BlankCheckEod none
      BarcodeLifecycleState.Available false CommittedCopyState.None =
      ok InitDecision.FreshInit := by
  rfl

theorem blank_assigned_barcode_is_anomaly (assigned_uuid : Std.U64) :
    decide_tape_init BotClassification.BlankCheckEod none
      (BarcodeLifecycleState.AssignedTo assigned_uuid) false CommittedCopyState.None =
      ok (InitDecision.Anomaly TapeInitError.BarcodeAssignedToDifferentUuid) := by
  rfl

theorem blank_retired_barcode_is_anomaly :
    decide_tape_init BotClassification.BlankCheckEod none BarcodeLifecycleState.Retired
      false CommittedCopyState.None =
      ok (InitDecision.Anomaly TapeInitError.BarcodeRetired) := by
  rfl

theorem blank_current_pool_committed_copies_refuse :
    decide_tape_init BotClassification.BlankCheckEod none
      BarcodeLifecycleState.Available false CommittedCopyState.CurrentPool =
      ok (InitDecision.RefuseClobber TapeInitError.CommittedCopiesPresent) := by
  rfl

theorem ours_clean_available_noop (uuid : Std.U64) (geometry : TapeInitGeometry) :
    decide_tape_init (BotClassification.OursBootstrap uuid geometry)
      (some (cleanRow uuid geometry)) BarcodeLifecycleState.Available false
      CommittedCopyState.None =
      ok InitDecision.IdempotentNoOp := by
  simp [decide_tape_init, committed_pool_conflict, decide_ours_init, cleanRow,
    disposition_is_retired, relation_is_unrecorded_relabel, geometry_matches,
    CommittedCopyState.has_committed_copies]

theorem ours_clean_assigned_same_uuid_noop (uuid : Std.U64)
    (geometry : TapeInitGeometry) :
    decide_tape_init (BotClassification.OursBootstrap uuid geometry)
      (some (cleanRow uuid geometry)) (BarcodeLifecycleState.AssignedTo uuid)
      false CommittedCopyState.None =
      ok InitDecision.IdempotentNoOp := by
  simp [decide_tape_init, committed_pool_conflict, decide_ours_init, cleanRow,
    disposition_is_retired, relation_is_unrecorded_relabel, geometry_matches,
    CommittedCopyState.has_committed_copies]

theorem ours_relabeled_matching_uuid_noop (uuid : Std.U64)
    (geometry : TapeInitGeometry) :
    decide_tape_init (BotClassification.OursBootstrap uuid geometry)
      (some (relabeledCleanRow uuid geometry))
      (BarcodeLifecycleState.AssignedTo uuid) false CommittedCopyState.None =
      ok InitDecision.IdempotentNoOp := by
  simp [decide_tape_init, committed_pool_conflict, decide_ours_init,
    relabeledCleanRow, cleanRow, disposition_is_retired,
    relation_is_unrecorded_relabel, geometry_matches,
    CommittedCopyState.has_committed_copies]

theorem ours_assigned_elsewhere_is_anomaly (bot_uuid assigned_uuid : Std.U64)
    (geometry : TapeInitGeometry) (catalog_row : Option CatalogTapeInitRow)
    (physical_data_past_bootstrap : Bool) (h : assigned_uuid ≠ bot_uuid) :
    decide_tape_init (BotClassification.OursBootstrap bot_uuid geometry)
      catalog_row (BarcodeLifecycleState.AssignedTo assigned_uuid)
      physical_data_past_bootstrap CommittedCopyState.None =
      ok (InitDecision.Anomaly TapeInitError.BarcodeAssignedToDifferentUuid) := by
  simp [decide_tape_init, committed_pool_conflict, decide_ours_init, h]

theorem ours_retired_barcode_is_anomaly (uuid : Std.U64)
    (geometry : TapeInitGeometry) (catalog_row : Option CatalogTapeInitRow)
    (physical_data_past_bootstrap : Bool) :
    decide_tape_init (BotClassification.OursBootstrap uuid geometry)
      catalog_row BarcodeLifecycleState.Retired physical_data_past_bootstrap
      CommittedCopyState.None =
      ok (InitDecision.Anomaly TapeInitError.BarcodeRetired) := by
  rfl

theorem ours_without_catalog_row_needs_rebuild (uuid : Std.U64)
    (geometry : TapeInitGeometry) :
    decide_tape_init (BotClassification.OursBootstrap uuid geometry) none
      BarcodeLifecycleState.Available false CommittedCopyState.None =
      ok (InitDecision.NeedsExplicitRebuild TapeInitError.MissingCatalogRow) := by
  rfl

theorem ours_catalog_uuid_mismatch_is_anomaly (bot_uuid row_uuid : Std.U64)
    (geometry : TapeInitGeometry) (h : row_uuid ≠ bot_uuid) :
    decide_tape_init (BotClassification.OursBootstrap bot_uuid geometry)
      (some (cleanRow row_uuid geometry)) BarcodeLifecycleState.Available false
      CommittedCopyState.None =
      ok (InitDecision.Anomaly TapeInitError.MediaSwapReusedBarcode) := by
  simp [decide_tape_init, committed_pool_conflict, decide_ours_init, cleanRow, h]

theorem ours_retired_catalog_row_is_fresh (uuid : Std.U64)
    (geometry : TapeInitGeometry) :
    decide_tape_init (BotClassification.OursBootstrap uuid geometry)
      (some (retiredRow uuid geometry)) BarcodeLifecycleState.Available true
      CommittedCopyState.None =
      ok InitDecision.FreshInit := by
  simp [decide_tape_init, committed_pool_conflict, decide_ours_init, retiredRow,
    disposition_is_retired]

theorem ours_unrecorded_relabel_is_anomaly (uuid : Std.U64)
    (geometry : TapeInitGeometry) :
    decide_tape_init (BotClassification.OursBootstrap uuid geometry)
      (some (unrecordedRelabelRow uuid geometry))
      (BarcodeLifecycleState.AssignedTo uuid) false CommittedCopyState.None =
      ok (InitDecision.Anomaly TapeInitError.BarcodeChangedWithoutRelabel) := by
  simp [decide_tape_init, committed_pool_conflict, decide_ours_init,
    unrecordedRelabelRow, cleanRow, disposition_is_retired,
    relation_is_unrecorded_relabel]

theorem ours_physical_data_past_bootstrap_refuses (uuid : Std.U64)
    (geometry : TapeInitGeometry) :
    decide_tape_init (BotClassification.OursBootstrap uuid geometry)
      (some (cleanRow uuid geometry)) (BarcodeLifecycleState.AssignedTo uuid)
      true CommittedCopyState.None =
      ok (InitDecision.RefuseClobber TapeInitError.PhysicalDataPastBootstrap) := by
  simp [decide_tape_init, committed_pool_conflict, decide_ours_init, cleanRow,
    disposition_is_retired, relation_is_unrecorded_relabel, refuse]

theorem ours_current_pool_committed_copies_refuse (uuid : Std.U64)
    (geometry : TapeInitGeometry) :
    decide_tape_init (BotClassification.OursBootstrap uuid geometry)
      (some (cleanRow uuid geometry)) (BarcodeLifecycleState.AssignedTo uuid)
      false CommittedCopyState.CurrentPool =
      ok (InitDecision.RefuseClobber TapeInitError.CommittedCopiesPresent) := by
  simp [decide_tape_init, committed_pool_conflict, decide_ours_init, cleanRow,
    disposition_is_retired, relation_is_unrecorded_relabel,
    CommittedCopyState.has_committed_copies, refuse]

theorem ours_geometry_mismatch_requires_force (uuid : Std.U64)
    (bot_geometry row_geometry : TapeInitGeometry)
    (hgeom : geometry_matches bot_geometry row_geometry = ok false) :
    decide_tape_init (BotClassification.OursBootstrap uuid bot_geometry)
      (some (cleanRow uuid row_geometry)) (BarcodeLifecycleState.AssignedTo uuid)
      false CommittedCopyState.None =
      ok (InitDecision.RequireForce TapeInitError.GeometryMismatch) := by
  simp [decide_tape_init, committed_pool_conflict, decide_ours_init, cleanRow,
    disposition_is_retired, relation_is_unrecorded_relabel,
    CommittedCopyState.has_committed_copies, hgeom]

theorem ours_written_catalog_row_refuses (uuid : Std.U64)
    (geometry : TapeInitGeometry) :
    decide_tape_init (BotClassification.OursBootstrap uuid geometry)
      (some (writtenRow uuid geometry)) (BarcodeLifecycleState.AssignedTo uuid)
      false CommittedCopyState.None =
      ok (InitDecision.RefuseClobber TapeInitError.CatalogIndicatesWritten) := by
  simp [decide_tape_init, committed_pool_conflict, decide_ours_init, writtenRow,
    cleanRow, disposition_is_retired, relation_is_unrecorded_relabel,
    geometry_matches, CommittedCopyState.has_committed_copies, refuse]

end TapeInit
