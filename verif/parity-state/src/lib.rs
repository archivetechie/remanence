//! Verification extraction of the object parity-state summary logic.
//!
//! This crate is a standalone, dependency-free copy of
//! `remanence-parity/src/model.rs`'s `ObjectParityState` /
//! `ObjectParityStateUpdateRange` decision logic, kept in the exact shape the
//! Charon → Aeneas → Lean pipeline can translate. Only the error type differs
//! from the original (`ParityError` here carries just the `Invariant` variant,
//! the only one these functions construct).
//!
//! The `drift_guard` test asserts the decision-logic snippets in this file are
//! byte-identical to the ones in `crates/remanence-parity/src/model.rs`; if
//! that test fails, the original moved and this extraction (and its Lean
//! proofs) must be re-synced.

/// Invariant-violation error, mirroring `ParityError::Invariant` — the only
/// variant the extracted functions can produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParityError {
    Invariant(&'static str),
}

impl core::fmt::Display for ParityError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Invariant(msg) => write!(f, "parity invariant violated: {msg}"),
        }
    }
}

/// Catalog-facing parity protection summary for one object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectParityState {
    /// No ordinal in the object is below the protection watermark yet.
    Pending,
    /// Some, but not all, object ordinals are below the watermark.
    Partial,
    /// The object's full half-open ordinal range is protected.
    Protected,
}

/// Catalog predicate for recomputing object parity states after a sidecar
/// advances the tape protection watermark (`docs/layer3c-design.md` §7.2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectParityStateUpdateRange {
    /// Previous `catalog_tapes.highest_protected_ordinal`.
    pub old_highest_protected_ordinal: u64,
    /// Newly committed `catalog_tapes.highest_protected_ordinal`.
    pub new_highest_protected_ordinal: u64,
}

impl ObjectParityStateUpdateRange {
    /// Build the catalog recomputation predicate for a monotonic watermark
    /// advance.
    pub fn from_watermark_advance(
        old_highest_protected_ordinal: u64,
        new_highest_protected_ordinal: u64,
    ) -> Result<Option<Self>, ParityError> {
        if new_highest_protected_ordinal < old_highest_protected_ordinal {
            return Err(ParityError::Invariant(
                "protection watermark cannot move backwards",
            ));
        }
        if new_highest_protected_ordinal == old_highest_protected_ordinal {
            return Ok(None);
        }
        Ok(Some(Self {
            old_highest_protected_ordinal,
            new_highest_protected_ordinal,
        }))
    }

    /// Upper-exclusive bound for `first_parity_data_ordinal`.
    pub fn first_parity_data_ordinal_upper_exclusive(self) -> u64 {
        self.new_highest_protected_ordinal
    }

    /// Lower-exclusive bound for `ordinal_end_exclusive`.
    pub fn ordinal_end_exclusive_lower_exclusive(self) -> u64 {
        self.old_highest_protected_ordinal
    }

    /// Whether the object row should be recomputed in the watermark-advance
    /// transaction.
    pub fn includes_object(
        self,
        first_parity_data_ordinal: u64,
        data_block_count: u64,
    ) -> Result<bool, ParityError> {
        if data_block_count == 0 {
            return Err(ParityError::Invariant(
                "object parity state update range requires at least one data block",
            ));
        }
        let ordinal_end_exclusive = first_parity_data_ordinal
            .checked_add(data_block_count)
            .ok_or(ParityError::Invariant("object ordinal range overflows"))?;

        Ok(
            first_parity_data_ordinal < self.first_parity_data_ordinal_upper_exclusive()
                && ordinal_end_exclusive > self.ordinal_end_exclusive_lower_exclusive(),
        )
    }

    /// Recompute the object state at the new watermark if this object falls in
    /// the affected range.
    pub fn recompute_object(
        self,
        first_parity_data_ordinal: u64,
        data_block_count: u64,
    ) -> Result<Option<ObjectParityState>, ParityError> {
        if !self.includes_object(first_parity_data_ordinal, data_block_count)? {
            return Ok(None);
        }
        ObjectParityState::from_ordinals(
            first_parity_data_ordinal,
            data_block_count,
            self.new_highest_protected_ordinal,
        )
        .map(Some)
    }
}

impl ObjectParityState {
    /// Derive the catalog summary from the object's ordinal range and the
    /// tape protection watermark (`docs/layer3c-design.md` §7.2.1 / §10.1).
    pub fn from_ordinals(
        first_parity_data_ordinal: u64,
        data_block_count: u64,
        highest_protected_ordinal: u64,
    ) -> Result<Self, ParityError> {
        if data_block_count == 0 {
            return Err(ParityError::Invariant(
                "object parity state requires at least one data block",
            ));
        }

        let ordinal_end_exclusive = first_parity_data_ordinal
            .checked_add(data_block_count)
            .ok_or(ParityError::Invariant("object ordinal range overflows"))?;

        if ordinal_end_exclusive <= highest_protected_ordinal {
            Ok(Self::Protected)
        } else if first_parity_data_ordinal >= highest_protected_ordinal {
            Ok(Self::Pending)
        } else {
            Ok(Self::Partial)
        }
    }

    /// Stable catalog string for `catalog_objects.parity_state`.
    pub fn as_catalog_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Partial => "partial",
            Self::Protected => "protected",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte-identical snippets that must appear in BOTH this file and the
    /// original `crates/remanence-parity/src/model.rs`. If the original
    /// changes, this fails and the extraction + Lean proofs must be re-synced.
    #[test]
    fn drift_guard() {
        let this_file = include_str!("lib.rs");
        let original = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/remanence-parity/src/model.rs"
        ))
        .expect("original model.rs must be readable from verif/parity-state");

        let snippets: &[&str] = &[
            // from_ordinals classification core
            "        if ordinal_end_exclusive <= highest_protected_ordinal {\n            Ok(Self::Protected)\n        } else if first_parity_data_ordinal >= highest_protected_ordinal {\n            Ok(Self::Pending)\n        } else {\n            Ok(Self::Partial)\n        }",
            // includes_object predicate core
            "            first_parity_data_ordinal < self.first_parity_data_ordinal_upper_exclusive()\n                && ordinal_end_exclusive > self.ordinal_end_exclusive_lower_exclusive(),",
            // watermark-advance guards
            "        if new_highest_protected_ordinal < old_highest_protected_ordinal {",
            "        if new_highest_protected_ordinal == old_highest_protected_ordinal {\n            return Ok(None);\n        }",
            // recompute_object delegation
            "        if !self.includes_object(first_parity_data_ordinal, data_block_count)? {\n            return Ok(None);\n        }\n        ObjectParityState::from_ordinals(\n            first_parity_data_ordinal,\n            data_block_count,\n            self.new_highest_protected_ordinal,\n        )\n        .map(Some)",
        ];
        for (i, snippet) in snippets.iter().enumerate() {
            assert!(
                this_file.contains(snippet),
                "snippet {i} missing from verif extraction"
            );
            assert!(
                original.contains(snippet),
                "snippet {i} no longer in remanence-parity model.rs — original \
                 changed; re-sync this extraction and its Lean proofs"
            );
        }
    }

    // Behavior locks copied from remanence-parity/src/model.rs.

    #[test]
    fn object_parity_state_matches_watermark_rules() {
        assert_eq!(
            ObjectParityState::from_ordinals(10, 5, 10).unwrap(),
            ObjectParityState::Pending,
            "first ordinal at the watermark means no block is protected yet"
        );
        assert_eq!(
            ObjectParityState::from_ordinals(10, 5, 12).unwrap(),
            ObjectParityState::Partial,
            "watermark inside the object range means only the prefix is protected"
        );
        assert_eq!(
            ObjectParityState::from_ordinals(10, 5, 15).unwrap(),
            ObjectParityState::Protected,
            "end-exclusive exactly at the watermark is fully protected"
        );
        assert_eq!(
            ObjectParityState::from_ordinals(10, 5, 20).unwrap(),
            ObjectParityState::Protected
        );
        assert_eq!(ObjectParityState::Partial.as_catalog_str(), "partial");
    }

    #[test]
    fn object_parity_state_rejects_invalid_catalog_ranges() {
        let zero = ObjectParityState::from_ordinals(10, 0, 10).unwrap_err();
        assert!(format!("{zero}").contains("at least one data block"));

        let overflow = ObjectParityState::from_ordinals(u64::MAX, 1, u64::MAX).unwrap_err();
        assert!(format!("{overflow}").contains("ordinal range overflows"));
    }

    #[test]
    fn object_parity_state_update_range_matches_catalog_predicate() {
        let range = ObjectParityStateUpdateRange::from_watermark_advance(4, 8)
            .unwrap()
            .expect("watermark advance creates a recompute range");
        assert_eq!(range.first_parity_data_ordinal_upper_exclusive(), 8);
        assert_eq!(range.ordinal_end_exclusive_lower_exclusive(), 4);

        assert!(
            !range.includes_object(0, 4).unwrap(),
            "object ending exactly at old W was already protected"
        );
        assert_eq!(
            range.recompute_object(4, 2).unwrap(),
            Some(ObjectParityState::Protected),
            "object starting at old W can become protected"
        );
        assert_eq!(
            range.recompute_object(6, 4).unwrap(),
            Some(ObjectParityState::Partial),
            "object straddling new W remains partial but is still affected"
        );
        assert!(
            !range.includes_object(8, 2).unwrap(),
            "object starting exactly at new W remains pending"
        );
        assert_eq!(
            range.recompute_object(0, 10).unwrap(),
            Some(ObjectParityState::Partial),
            "already-partial large object still matches because its covered prefix grew"
        );
    }

    #[test]
    fn object_parity_state_update_range_rejects_bad_advances_and_ranges() {
        assert_eq!(
            ObjectParityStateUpdateRange::from_watermark_advance(8, 8).unwrap(),
            None
        );

        let backwards = ObjectParityStateUpdateRange::from_watermark_advance(8, 4).unwrap_err();
        assert!(
            format!("{backwards}").contains("cannot move backwards"),
            "{backwards}"
        );

        let range = ObjectParityStateUpdateRange::from_watermark_advance(4, 8)
            .unwrap()
            .unwrap();
        let zero = range.includes_object(4, 0).unwrap_err();
        assert!(format!("{zero}").contains("at least one data block"));
    }
}
