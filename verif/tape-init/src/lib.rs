//! Verification extraction of the tape-init safety decision core.
//!
//! This crate is a standalone, dependency-free model of
//! `crates/remanence-api/src/tape_init.rs`'s `decide_tape_init` branch logic.
//! It preserves the production decision ordering while replacing production
//! payloads outside the Aeneas subset with compact proof-facing values:
//! UUIDs become equality-only `u64`s, geometry keeps only compared fields,
//! foreign-format/pool payload strings become named categories, and all error
//! payloads become stable reason variants. The `drift_guard` test pins the
//! production snippets this extraction mirrors; if it fails, the extraction and
//! Lean proofs must be re-synced.

pub type TapeUuid = u64;

#[derive(Clone, Copy)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub struct TapeInitGeometry {
    pub block_size_bytes: u64,
    pub parity_mode: u64,
}

#[derive(Clone, Copy)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub enum BotClassification {
    BlankCheckEod,
    OursBootstrap {
        uuid: TapeUuid,
        geometry: TapeInitGeometry,
    },
    ForeignFormat,
    ReadError,
    UnrecognizedData,
}

#[derive(Clone, Copy)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub enum BarcodeLifecycleState {
    Available,
    AssignedTo(TapeUuid),
    Retired,
}

#[derive(Clone, Copy)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub enum CatalogBarcodeRelation {
    Matches,
    DiffersWithRecordedRelabel,
    DiffersWithoutRelabel,
}

#[derive(Clone, Copy)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub enum CatalogRowDisposition {
    Active,
    Retired,
}

#[derive(Clone, Copy)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub struct CatalogTapeInitRow {
    pub uuid: TapeUuid,
    pub geometry: TapeInitGeometry,
    pub catalog_unwritten: bool,
    pub barcode_relation: CatalogBarcodeRelation,
    pub disposition: CatalogRowDisposition,
}

#[derive(Clone, Copy)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub enum CommittedCopyState {
    None,
    CurrentPool,
    ForeignPool,
    UnknownPool,
}

impl CommittedCopyState {
    pub fn has_committed_copies(self) -> bool {
        !matches!(self, Self::None)
    }
}

#[derive(Clone, Copy)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub enum TapeInitError {
    BarcodeAssignedToDifferentUuid,
    BarcodeRetired,
    MediaSwapReusedBarcode,
    BarcodeChangedWithoutRelabel,
    BotReadError,
    ForeignFormat,
    UnrecognizedData,
    MissingCatalogRow,
    PhysicalDataPastBootstrap,
    CommittedCopiesPresent,
    TapePoolAssignmentConflict,
    GeometryMismatch,
    CatalogIndicatesWritten,
}

#[derive(Clone, Copy)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub enum InitDecision {
    FreshInit,
    IdempotentNoOp,
    RequireForce { reason: TapeInitError },
    RefuseClobber { reason: TapeInitError },
    Anomaly { reason: TapeInitError },
    NeedsExplicitRebuild { reason: TapeInitError },
}

pub fn decide_tape_init(
    bot: BotClassification,
    catalog_row: Option<CatalogTapeInitRow>,
    barcode_state: BarcodeLifecycleState,
    physical_data_past_bootstrap: bool,
    committed_copies: CommittedCopyState,
) -> InitDecision {
    if let Some(reason) = committed_pool_conflict(committed_copies) {
        return InitDecision::Anomaly { reason };
    }

    match bot {
        BotClassification::ReadError => refuse(TapeInitError::BotReadError),
        BotClassification::ForeignFormat => refuse(TapeInitError::ForeignFormat),
        BotClassification::UnrecognizedData => refuse(TapeInitError::UnrecognizedData),
        BotClassification::BlankCheckEod => decide_blank_init(barcode_state, committed_copies),
        BotClassification::OursBootstrap { uuid, geometry } => decide_ours_init(
            uuid,
            geometry,
            catalog_row,
            barcode_state,
            physical_data_past_bootstrap,
            committed_copies,
        ),
    }
}

pub fn decide_blank_init(
    barcode_state: BarcodeLifecycleState,
    committed_copies: CommittedCopyState,
) -> InitDecision {
    match barcode_state {
        BarcodeLifecycleState::Available => {}
        BarcodeLifecycleState::AssignedTo(_) => {
            return InitDecision::Anomaly {
                reason: TapeInitError::BarcodeAssignedToDifferentUuid,
            };
        }
        BarcodeLifecycleState::Retired => {
            return InitDecision::Anomaly {
                reason: TapeInitError::BarcodeRetired,
            };
        }
    }
    if committed_copies.has_committed_copies() {
        return refuse(TapeInitError::CommittedCopiesPresent);
    }
    InitDecision::FreshInit
}

pub fn decide_ours_init(
    bot_uuid: TapeUuid,
    bot_geometry: TapeInitGeometry,
    catalog_row: Option<CatalogTapeInitRow>,
    barcode_state: BarcodeLifecycleState,
    physical_data_past_bootstrap: bool,
    committed_copies: CommittedCopyState,
) -> InitDecision {
    match barcode_state {
        BarcodeLifecycleState::Available => {}
        BarcodeLifecycleState::AssignedTo(assigned_uuid) if assigned_uuid == bot_uuid => {}
        BarcodeLifecycleState::AssignedTo(_) => {
            return InitDecision::Anomaly {
                reason: TapeInitError::BarcodeAssignedToDifferentUuid,
            };
        }
        BarcodeLifecycleState::Retired => {
            return InitDecision::Anomaly {
                reason: TapeInitError::BarcodeRetired,
            };
        }
    }

    let catalog_row = match catalog_row {
        Some(row) => row,
        None => {
            return InitDecision::NeedsExplicitRebuild {
                reason: TapeInitError::MissingCatalogRow,
            };
        }
    };

    if catalog_row.uuid != bot_uuid {
        return InitDecision::Anomaly {
            reason: TapeInitError::MediaSwapReusedBarcode,
        };
    }
    if disposition_is_retired(catalog_row.disposition) {
        return InitDecision::FreshInit;
    }
    if relation_is_unrecorded_relabel(catalog_row.barcode_relation) {
        return InitDecision::Anomaly {
            reason: TapeInitError::BarcodeChangedWithoutRelabel,
        };
    }
    if physical_data_past_bootstrap {
        return refuse(TapeInitError::PhysicalDataPastBootstrap);
    }
    if committed_copies.has_committed_copies() {
        return refuse(TapeInitError::CommittedCopiesPresent);
    }
    if !geometry_matches(bot_geometry, catalog_row.geometry) {
        return InitDecision::RequireForce {
            reason: TapeInitError::GeometryMismatch,
        };
    }
    if !catalog_row.catalog_unwritten {
        return refuse(TapeInitError::CatalogIndicatesWritten);
    }
    InitDecision::IdempotentNoOp
}

pub fn committed_pool_conflict(committed_copies: CommittedCopyState) -> Option<TapeInitError> {
    match committed_copies {
        CommittedCopyState::None => None,
        CommittedCopyState::CurrentPool => None,
        CommittedCopyState::ForeignPool => Some(TapeInitError::TapePoolAssignmentConflict),
        CommittedCopyState::UnknownPool => Some(TapeInitError::TapePoolAssignmentConflict),
    }
}

pub fn geometry_matches(left: TapeInitGeometry, right: TapeInitGeometry) -> bool {
    left.block_size_bytes == right.block_size_bytes && left.parity_mode == right.parity_mode
}

pub fn disposition_is_retired(disposition: CatalogRowDisposition) -> bool {
    matches!(disposition, CatalogRowDisposition::Retired)
}

pub fn relation_is_unrecorded_relabel(relation: CatalogBarcodeRelation) -> bool {
    matches!(relation, CatalogBarcodeRelation::DiffersWithoutRelabel)
}

pub fn refuse(reason: TapeInitError) -> InitDecision {
    InitDecision::RefuseClobber { reason }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOT_UUID: TapeUuid = 1;
    const OTHER_UUID: TapeUuid = 2;

    fn geometry() -> TapeInitGeometry {
        TapeInitGeometry {
            block_size_bytes: 4096,
            parity_mode: 1,
        }
    }

    fn other_geometry() -> TapeInitGeometry {
        TapeInitGeometry {
            block_size_bytes: 8192,
            parity_mode: 1,
        }
    }

    fn ours_bot() -> BotClassification {
        BotClassification::OursBootstrap {
            uuid: BOT_UUID,
            geometry: geometry(),
        }
    }

    fn catalog_row() -> CatalogTapeInitRow {
        CatalogTapeInitRow {
            uuid: BOT_UUID,
            geometry: geometry(),
            catalog_unwritten: true,
            barcode_relation: CatalogBarcodeRelation::Matches,
            disposition: CatalogRowDisposition::Active,
        }
    }

    fn retired_catalog_row() -> CatalogTapeInitRow {
        CatalogTapeInitRow {
            uuid: BOT_UUID,
            geometry: geometry(),
            catalog_unwritten: false,
            barcode_relation: CatalogBarcodeRelation::DiffersWithoutRelabel,
            disposition: CatalogRowDisposition::Retired,
        }
    }

    #[test]
    fn drift_guard() {
        let this_file = include_str!("lib.rs");
        let original = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/remanence-api/src/tape_init.rs"
        ))
        .expect("original tape_init.rs must be readable from verif/tape-init");

        let snippets: &[&str] = &[
            "if let Some(reason) = committed_pool_conflict(committed_copies, derived_pool) {",
            "BotClassification::ReadError => refuse(TapeInitError::BotReadError),",
            "BotClassification::ForeignFormat { name } => {\n            refuse(TapeInitError::ForeignFormat(name.clone()))\n        }",
            "BotClassification::UnrecognizedData => refuse(TapeInitError::UnrecognizedData),",
            "BotClassification::BlankCheckEod => decide_blank_init(barcode_state, committed_copies),",
            "BotClassification::OursBootstrap { uuid, geometry } => decide_ours_init(",
            "BarcodeLifecycleState::AssignedTo(assigned_uuid) if *assigned_uuid == bot_uuid => {}",
            "let Some(catalog_row) = catalog_row else {",
            "if catalog_row.uuid != bot_uuid {",
            "if catalog_row.disposition == CatalogRowDisposition::Retired {",
            "if catalog_row.barcode_relation == CatalogBarcodeRelation::DiffersWithoutRelabel {",
            "if physical_data_past_bootstrap {",
            "if committed_copies.has_committed_copies() {",
            "if bot_geometry != &catalog_row.geometry {",
            "if !catalog_row.catalog_unwritten {",
            "CommittedCopyState::Pool(pool_id) if pool_id == derived_pool => None,",
            "CommittedCopyState::UnknownPool => Some(TapeInitError::TapePoolAssignmentConflict {",
        ];
        for (i, snippet) in snippets.iter().enumerate() {
            assert!(
                original.contains(snippet),
                "snippet {i} no longer in remanence-api tape_init.rs -- original \
                 changed; re-sync this extraction and its Lean proofs"
            );
        }

        let extraction_snippets: &[&str] = &[
            "if let Some(reason) = committed_pool_conflict(committed_copies) {",
            "BotClassification::BlankCheckEod => decide_blank_init(barcode_state, committed_copies),",
            "BarcodeLifecycleState::AssignedTo(assigned_uuid) if assigned_uuid == bot_uuid => {}",
            "if catalog_row.uuid != bot_uuid {",
            "if disposition_is_retired(catalog_row.disposition) {",
            "if relation_is_unrecorded_relabel(catalog_row.barcode_relation) {",
            "if physical_data_past_bootstrap {",
            "if committed_copies.has_committed_copies() {",
            "if !geometry_matches(bot_geometry, catalog_row.geometry) {",
            "CommittedCopyState::ForeignPool => Some(TapeInitError::TapePoolAssignmentConflict),",
        ];
        for (i, snippet) in extraction_snippets.iter().enumerate() {
            assert!(
                this_file.contains(snippet),
                "extraction snippet {i} missing from verif tape-init model"
            );
        }
    }

    #[test]
    fn blank_check_available_barcode_is_fresh_init() {
        assert_eq!(
            decide_tape_init(
                BotClassification::BlankCheckEod,
                None,
                BarcodeLifecycleState::Available,
                false,
                CommittedCopyState::None,
            ),
            InitDecision::FreshInit
        );
    }

    #[test]
    fn read_error_refuses_fail_closed() {
        assert_eq!(
            decide_tape_init(
                BotClassification::ReadError,
                None,
                BarcodeLifecycleState::Available,
                false,
                CommittedCopyState::None,
            ),
            InitDecision::RefuseClobber {
                reason: TapeInitError::BotReadError,
            }
        );
    }

    #[test]
    fn non_remanence_data_refuses() {
        assert_eq!(
            decide_tape_init(
                BotClassification::ForeignFormat,
                None,
                BarcodeLifecycleState::Available,
                false,
                CommittedCopyState::None,
            ),
            InitDecision::RefuseClobber {
                reason: TapeInitError::ForeignFormat,
            }
        );
        assert_eq!(
            decide_tape_init(
                BotClassification::UnrecognizedData,
                None,
                BarcodeLifecycleState::Available,
                false,
                CommittedCopyState::None,
            ),
            InitDecision::RefuseClobber {
                reason: TapeInitError::UnrecognizedData,
            }
        );
    }

    #[test]
    fn committed_pool_conflict_precedes_bot_and_barcode_state() {
        assert_eq!(
            decide_tape_init(
                BotClassification::ReadError,
                Some(catalog_row()),
                BarcodeLifecycleState::Retired,
                true,
                CommittedCopyState::ForeignPool,
            ),
            InitDecision::Anomaly {
                reason: TapeInitError::TapePoolAssignmentConflict,
            }
        );
    }

    #[test]
    fn matching_unwritten_bootstrap_is_idempotent_noop() {
        assert_eq!(
            decide_tape_init(
                ours_bot(),
                Some(catalog_row()),
                BarcodeLifecycleState::AssignedTo(BOT_UUID),
                false,
                CommittedCopyState::None,
            ),
            InitDecision::IdempotentNoOp
        );
    }

    #[test]
    fn relabeled_matching_uuid_can_noop_when_relabel_record_exists() {
        let mut row = catalog_row();
        row.barcode_relation = CatalogBarcodeRelation::DiffersWithRecordedRelabel;

        assert_eq!(
            decide_tape_init(
                ours_bot(),
                Some(row),
                BarcodeLifecycleState::AssignedTo(BOT_UUID),
                false,
                CommittedCopyState::None,
            ),
            InitDecision::IdempotentNoOp
        );
    }

    #[test]
    fn barcode_change_without_relabel_is_anomaly() {
        let mut row = catalog_row();
        row.barcode_relation = CatalogBarcodeRelation::DiffersWithoutRelabel;

        assert_eq!(
            decide_tape_init(
                ours_bot(),
                Some(row),
                BarcodeLifecycleState::AssignedTo(BOT_UUID),
                false,
                CommittedCopyState::None,
            ),
            InitDecision::Anomaly {
                reason: TapeInitError::BarcodeChangedWithoutRelabel,
            }
        );
    }

    #[test]
    fn physical_data_past_bootstrap_refuses_even_without_committed_copies() {
        assert_eq!(
            decide_tape_init(
                ours_bot(),
                Some(catalog_row()),
                BarcodeLifecycleState::AssignedTo(BOT_UUID),
                true,
                CommittedCopyState::None,
            ),
            InitDecision::RefuseClobber {
                reason: TapeInitError::PhysicalDataPastBootstrap,
            }
        );
    }

    #[test]
    fn retired_row_with_available_barcode_is_fresh_init_despite_physical_data() {
        assert_eq!(
            decide_tape_init(
                ours_bot(),
                Some(retired_catalog_row()),
                BarcodeLifecycleState::Available,
                true,
                CommittedCopyState::None,
            ),
            InitDecision::FreshInit
        );
    }

    #[test]
    fn ours_without_catalog_row_needs_explicit_rebuild() {
        assert_eq!(
            decide_tape_init(
                ours_bot(),
                None,
                BarcodeLifecycleState::Available,
                false,
                CommittedCopyState::None,
            ),
            InitDecision::NeedsExplicitRebuild {
                reason: TapeInitError::MissingCatalogRow,
            }
        );
    }

    #[test]
    fn clean_geometry_mismatch_requires_scoped_force() {
        let mut row = catalog_row();
        row.geometry = other_geometry();

        assert_eq!(
            decide_tape_init(
                ours_bot(),
                Some(row),
                BarcodeLifecycleState::AssignedTo(BOT_UUID),
                false,
                CommittedCopyState::None,
            ),
            InitDecision::RequireForce {
                reason: TapeInitError::GeometryMismatch,
            }
        );
    }

    #[test]
    fn committed_copies_refuse_plain_init_after_pool_match() {
        assert_eq!(
            decide_tape_init(
                ours_bot(),
                Some(catalog_row()),
                BarcodeLifecycleState::AssignedTo(BOT_UUID),
                false,
                CommittedCopyState::CurrentPool,
            ),
            InitDecision::RefuseClobber {
                reason: TapeInitError::CommittedCopiesPresent,
            }
        );
    }

    #[test]
    fn assigned_barcode_for_different_uuid_is_anomaly() {
        assert_eq!(
            decide_tape_init(
                ours_bot(),
                Some(catalog_row()),
                BarcodeLifecycleState::AssignedTo(OTHER_UUID),
                false,
                CommittedCopyState::None,
            ),
            InitDecision::Anomaly {
                reason: TapeInitError::BarcodeAssignedToDifferentUuid,
            }
        );
    }

    #[test]
    fn catalog_written_row_refuses_without_force_path() {
        let mut row = catalog_row();
        row.catalog_unwritten = false;

        assert_eq!(
            decide_tape_init(
                ours_bot(),
                Some(row),
                BarcodeLifecycleState::AssignedTo(BOT_UUID),
                false,
                CommittedCopyState::None,
            ),
            InitDecision::RefuseClobber {
                reason: TapeInitError::CatalogIndicatesWritten,
            }
        );
    }
}
