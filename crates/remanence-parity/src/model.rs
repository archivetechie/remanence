//! Value types for Layer 3c — `ParityScheme`, `SchemeId`,
//! `StripeAddress`, `StripePosition`, `RecoveryEvent`,
//! `RecoveryOutcome`, `SidecarMetadataHealth`,
//! `ObjectParityState`, `ObjectParityStateUpdateRange`, `FinalGeometry`,
//! and read-path audit events.
//!
//! See `docs/layer3c-design.md` for the active v0.4.4 sidecar-only design.

use std::borrow::Cow;

/// Stable identifier for a parity scheme. Format:
/// `"rs-cauchy-gf256-v1"` for the initial scheme. Once a tape is
/// written with scheme S, reading it requires exactly S, so
/// scheme IDs are forever. Algorithm or parameter-range changes
/// get a new ID; the old scheme stays registered as long as any
/// tape uses it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SchemeId(Cow<'static, str>);

impl SchemeId {
    /// Construct an ID from a `&'static str` literal (no
    /// allocation). For built-in schemes.
    pub fn new_static(id: &'static str) -> Self {
        Self(Cow::Borrowed(id))
    }

    /// Construct an ID from an owned string. Used when reading
    /// a bootstrap whose scheme ID isn't one of the in-tree
    /// constants.
    pub fn new_owned(id: String) -> Self {
        Self(Cow::Owned(id))
    }

    /// The wire-format string. Stable on tape forever.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SchemeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Parity scheme — the configuration the writer uses and the
/// bootstrap records on tape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParityScheme {
    /// Scheme identifier (see [`SchemeId`]).
    pub id: SchemeId,
    /// Data blocks per stripe (k). Codeword data size.
    pub data_blocks_per_stripe: u16,
    /// Parity blocks per stripe (m). Each stripe survives up to
    /// `m` block erasures.
    pub parity_blocks_per_stripe: u16,
    /// Stripes per neighborhood. Determines the LBA interleave
    /// pattern and the per-neighborhood damage-tolerance window.
    pub stripes_per_neighborhood: u32,
}

impl ParityScheme {
    /// Total blocks per neighborhood:
    /// `stripes_per_neighborhood × (k + m)`. Wider type than the
    /// individual fields because realistic schemes go up to
    /// hundreds of thousands of blocks per neighborhood.
    pub fn neighborhood_blocks(&self) -> u64 {
        self.stripes_per_neighborhood as u64
            * (self.data_blocks_per_stripe as u64 + self.parity_blocks_per_stripe as u64)
    }

    /// Parity bytes per usable data byte.
    ///
    /// Operator-facing capacity planning should use this ratio when answering
    /// "how much extra tape do I need for a given amount of user data?" For
    /// `k=128,m=4`, this is `4/128 = 3.125%`.
    pub fn parity_over_data_ratio(&self) -> f64 {
        if self.data_blocks_per_stripe == 0 {
            0.0
        } else {
            self.parity_blocks_per_stripe as f64 / self.data_blocks_per_stripe as f64
        }
    }

    /// Parity bytes as a fraction of all data+parity bytes written.
    ///
    /// Use this ratio for "what fraction of bytes already written are parity?"
    /// For `k=128,m=4`, this is `4/(128+4) = 3.0303%`.
    pub fn parity_fraction_of_total_written(&self) -> f64 {
        let total = self.data_blocks_per_stripe as u32 + self.parity_blocks_per_stripe as u32;
        if total == 0 {
            0.0
        } else {
            self.parity_blocks_per_stripe as f64 / total as f64
        }
    }

    /// Capacity overhead as a fraction of usable capacity.
    ///
    /// Compatibility alias for [`Self::parity_over_data_ratio`]. New
    /// operator-facing code should use the explicit method name to avoid
    /// mixing the two v0.7.2 overhead ratios.
    /// E.g. `m=4, k=128 → 4/128 = 0.03125 = 3.125%`. Returns 0.0
    /// if `k = 0` (which `validate` rejects, but the math is
    /// still defined to avoid a NaN surprise).
    pub fn overhead_ratio(&self) -> f64 {
        self.parity_over_data_ratio()
    }

    /// Maximum contiguous damage (in blocks) one neighborhood
    /// can recover from, assuming damage hits stripe positions
    /// roughly uniformly within the neighborhood. Equal to
    /// `stripes_per_neighborhood × m`. Per
    /// `docs/layer3c-design-v0.2.md` §5.2: damage up to S×m
    /// blocks affects at most `m` blocks per stripe and all
    /// stripes recover.
    pub fn contiguous_damage_threshold(&self) -> u64 {
        self.stripes_per_neighborhood as u64 * self.parity_blocks_per_stripe as u64
    }

    /// Validate the scheme parameters against
    /// `docs/layer3c-design-v0.2.md` §11.3 constraints. Returns
    /// `Ok(&self)` on success or a descriptive
    /// [`crate::ParityError::InvalidScheme`] on failure.
    ///
    /// Constraints:
    /// - `data_blocks_per_stripe >= 2` (k=1 is just replication).
    /// - `parity_blocks_per_stripe >= 1` (m=0 → use the
    ///   no-parity flag, not this struct).
    /// - `parity_blocks_per_stripe <= data_blocks_per_stripe`
    ///   (m > k would be wasteful — m copies of the data would
    ///   be smaller).
    /// - `stripes_per_neighborhood >= 1`.
    /// - `data_blocks_per_stripe + parity_blocks_per_stripe <=
    ///   255` so the GF(2⁸) Reed-Solomon library accepts the
    ///   stripe.
    pub fn validate(&self) -> Result<&Self, crate::error::ParityError> {
        if self.data_blocks_per_stripe < 2 {
            return Err(crate::error::ParityError::InvalidScheme(format!(
                "data_blocks_per_stripe = {} (must be >= 2)",
                self.data_blocks_per_stripe
            )));
        }
        if self.parity_blocks_per_stripe < 1 {
            return Err(crate::error::ParityError::InvalidScheme(
                "parity_blocks_per_stripe = 0 — use the bootstrap no-parity flag instead".into(),
            ));
        }
        if self.parity_blocks_per_stripe > self.data_blocks_per_stripe {
            return Err(crate::error::ParityError::InvalidScheme(format!(
                "parity_blocks_per_stripe = {} > data_blocks_per_stripe = {} (use replication)",
                self.parity_blocks_per_stripe, self.data_blocks_per_stripe
            )));
        }
        if self.stripes_per_neighborhood < 1 {
            return Err(crate::error::ParityError::InvalidScheme(
                "stripes_per_neighborhood = 0".into(),
            ));
        }
        let stripe_width =
            self.data_blocks_per_stripe as u32 + self.parity_blocks_per_stripe as u32;
        if stripe_width > 255 {
            return Err(crate::error::ParityError::InvalidScheme(format!(
                "k + m = {stripe_width} > 255 — GF(2^8) RS limit"
            )));
        }
        // Per design §11.3: total neighborhood blocks must fit
        // in u32 (4 G blocks). Mapping / writer code uses u32
        // counters internally and would overflow above the
        // bound. Codex 00:10 idref=36acce12 Medium catch.
        let neighborhood = self.neighborhood_blocks();
        if neighborhood > u32::MAX as u64 {
            return Err(crate::error::ParityError::InvalidScheme(format!(
                "neighborhood_blocks = S × (k + m) = {neighborhood} > u32::MAX = 4 G blocks \
                 (per docs/layer3c-design-v0.2.md §11.3)"
            )));
        }
        Ok(self)
    }
}

/// The result of mapping a physical tape LBA back to its parity
/// stripe identity. Reversible — given a `StripeAddress` and the
/// scheme, the LBA can be recomputed.
///
/// In the v0.4.4 sidecar path, the same value type is also used for
/// ordinal-space mapping: [`Self::neighborhood`] carries the parity epoch id,
/// and [`Self::position`] is always [`StripePosition::Data`] for a
/// `ParityDataOrdinal`. Parity shards are addressed in the sidecar index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StripeAddress {
    /// Legacy inline neighborhood index, or v0.4.4 parity epoch id.
    pub neighborhood: u64,
    /// Stripe index within the neighborhood
    /// (`0..stripes_per_neighborhood`).
    pub stripe_index: u32,
    /// Position within the stripe — data block or parity block.
    pub position: StripePosition,
}

/// Whether a block in a parity stripe is data or parity, and its
/// index within the stripe's data row (0..k) or parity row
/// (0..m).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StripePosition {
    /// Data block at index 0..k within its stripe.
    Data {
        /// Index within the stripe's data row (0..k).
        index: u16,
    },
    /// Parity block at index 0..m within its stripe.
    Parity {
        /// Index within the stripe's parity row (0..m).
        index: u16,
    },
}

/// Emitted by sidecar recovery on every recovery attempt. Layer 5
/// wires this through the audit hook so operators see recovery
/// events in the audit log — a tape that produces them should
/// be flagged for replacement.
#[derive(Clone, Debug)]
pub struct RecoveryEvent {
    /// Which stripe needed reconstruction.
    pub stripe: StripeAddress,
    /// Block positions within the stripe that were missing /
    /// unreadable.
    pub lost_blocks: Vec<StripePosition>,
    /// Whether reconstruction succeeded or the damage exceeded
    /// the scheme's tolerance.
    pub outcome: RecoveryOutcome,
    /// The LBA the caller originally asked to read — useful for
    /// audit-log correlation.
    ///
    /// Mirrors the body-LBA component of [`Self::at_requested`] for log
    /// consumers that still index read faults by object-local LBA.
    pub at_lba_requested: u64,
    /// Object-scoped address the caller asked for.
    ///
    /// Layer 3c v0.4.4 audit events are keyed by
    /// `(tape_file_number, body_lba)`.
    pub at_requested: (u32, u64),
}

/// Emitted when an object read sees a transport fault and the mandated single
/// retry succeeds without sidecar reconstruction.
#[derive(Clone, Debug)]
pub struct TransportRetryEvent {
    /// Object-scoped address the caller asked for.
    pub at_requested: (u32, u64),
    /// Object-local LBA the caller asked for.
    pub at_lba_requested: u64,
    /// Physical LBA retried after repositioning through the filemark map.
    pub physical_lba: u64,
    /// Tape partition of the retried physical position.
    pub partition: u32,
}

/// Health of the replicated sidecar metadata copies observed while opening a
/// sidecar for recovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SidecarMetadataHealth {
    /// Primary and tail header/index copies were both usable.
    BothCopiesUsable,
    /// The primary copy was usable, but the tail copy was unavailable.
    ///
    /// This is the addendum's `SidecarMetadataCopyLost` audit case.
    TailCopyLost,
    /// The tail copy was usable, but the primary copy was unavailable.
    ///
    /// This is the addendum's `SidecarPrimaryHeaderLost` audit case.
    PrimaryHeaderLost,
}

impl SidecarMetadataHealth {
    /// Whether the sidecar remained usable only because one replicated metadata
    /// copy survived.
    pub fn is_degraded(self) -> bool {
        self != Self::BothCopiesUsable
    }
}

/// Audit event emitted when a map-valid sidecar is usable for recovery but one
/// of its replicated metadata copies is unavailable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidecarMetadataHealthEvent {
    /// Parity sidecar tape file whose metadata health was observed.
    pub sidecar_tape_file_number: u32,
    /// Parity epoch protected by the sidecar.
    pub epoch_id: u64,
    /// Which replicated metadata-copy degradation was observed.
    pub health: SidecarMetadataHealth,
}

/// Recovery outcome paired with the affected stripe in a
/// [`RecoveryEvent`].
#[derive(Clone, Debug)]
pub enum RecoveryOutcome {
    /// Reconstructed successfully from `k` surviving blocks.
    Recovered,
    /// More than `m` blocks lost; reconstruction failed.
    Unrecoverable {
        /// How many blocks were missing.
        lost_count: u16,
    },
}

/// Catalog-facing parity protection summary for one object.
///
/// Layer 5 stores this as the operator-facing `parity_state` column. The
/// recovery path still uses the per-block watermark test directly; this enum
/// is only the object-level summary derived from an object's ordinal range and
/// the tape's `highest_protected_ordinal`.
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
/// advances the tape protection watermark.
///
/// Layer 5 can translate this directly into the range update required by
/// `docs/layer3c-design.md` §7.2.1:
///
/// ```text
/// first_parity_data_ordinal < first_parity_data_ordinal_upper_exclusive
/// AND ordinal_end_exclusive > ordinal_end_exclusive_lower_exclusive
/// ```
///
/// Objects matching that predicate were not fully protected at the old
/// watermark and have at least one ordinal below the new watermark. Recomputing
/// them with [`ObjectParityState::from_ordinals`] may still yield the same
/// string state for a large already-partial object, but the predicate is safe
/// for a single catalog transaction because it never misses an object whose
/// summary state can change.
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
    ///
    /// Returns `Ok(None)` when the watermark did not move and no objects need
    /// to be touched. A lower new watermark is rejected because sidecar commits
    /// must be monotonic.
    pub fn from_watermark_advance(
        old_highest_protected_ordinal: u64,
        new_highest_protected_ordinal: u64,
    ) -> Result<Option<Self>, crate::error::ParityError> {
        if new_highest_protected_ordinal < old_highest_protected_ordinal {
            return Err(crate::error::ParityError::Invariant(
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
    ) -> Result<bool, crate::error::ParityError> {
        if data_block_count == 0 {
            return Err(crate::error::ParityError::Invariant(
                "object parity state update range requires at least one data block",
            ));
        }
        let ordinal_end_exclusive = first_parity_data_ordinal
            .checked_add(data_block_count)
            .ok_or(crate::error::ParityError::Invariant(
                "object ordinal range overflows",
            ))?;

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
    ) -> Result<Option<ObjectParityState>, crate::error::ParityError> {
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
    /// tape protection watermark.
    ///
    /// This implements `docs/layer3c-design.md` §7.2.1 / §10.1:
    /// `protected` iff `ordinal_end_exclusive <= W`, `pending` iff
    /// `first_parity_data_ordinal >= W`, and `partial` otherwise.
    pub fn from_ordinals(
        first_parity_data_ordinal: u64,
        data_block_count: u64,
        highest_protected_ordinal: u64,
    ) -> Result<Self, crate::error::ParityError> {
        if data_block_count == 0 {
            return Err(crate::error::ParityError::Invariant(
                "object parity state requires at least one data block",
            ));
        }

        let ordinal_end_exclusive = first_parity_data_ordinal
            .checked_add(data_block_count)
            .ok_or(crate::error::ParityError::Invariant(
                "object ordinal range overflows",
            ))?;

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

/// What the writer needs to know after
/// [`ParitySink::finish`](crate::sink::ParitySink::finish).
#[derive(Clone, Debug)]
pub struct FinalGeometry {
    /// Logical LBA immediately after the last user-data block.
    ///
    /// Current sidecar finalization either commits protection for all accepted
    /// object data or fails and poisons the writer; it does not return a
    /// partially protected final geometry.
    pub data_area_end_lba: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        conservative_scheme, conservative_scheme_for_block_size, default_scheme,
        default_scheme_for_block_size, SCHEME_ID_RS_CAUCHY_GF256_V1,
    };

    #[test]
    fn scheme_id_static_and_owned_roundtrip() {
        let a = SchemeId::new_static("rs-cauchy-gf256-v1");
        let b = SchemeId::new_owned("rs-cauchy-gf256-v1".to_string());
        assert_eq!(a, b);
        assert_eq!(a.as_str(), "rs-cauchy-gf256-v1");
        assert_eq!(format!("{a}"), "rs-cauchy-gf256-v1");
    }

    #[test]
    fn default_scheme_matches_design_doc() {
        let s = default_scheme();
        assert_eq!(s.id.as_str(), SCHEME_ID_RS_CAUCHY_GF256_V1);
        assert_eq!(s.data_blocks_per_stripe, 128);
        assert_eq!(s.parity_blocks_per_stripe, 4);
        assert_eq!(s.stripes_per_neighborhood, 512);
        // 512 stripes × (128 + 4) blocks = 67,584 blocks.
        assert_eq!(s.neighborhood_blocks(), 67_584);
        // m/k = 4/128 = 0.03125 = 3.125% overhead.
        assert!((s.overhead_ratio() - 0.03125).abs() < 1e-9);
        assert!((s.parity_over_data_ratio() - 0.03125).abs() < 1e-9);
        assert!((s.parity_fraction_of_total_written() - (4.0 / 132.0)).abs() < 1e-9);
        // S × m = 512 × 4 = 2,048 blocks contiguous damage tolerance
        // at rao-v1's 256 KiB default block size (~512 MiB).
        assert_eq!(s.contiguous_damage_threshold(), 2_048);
    }

    #[test]
    fn conservative_scheme_matches_design_doc() {
        let s = conservative_scheme();
        assert_eq!(s.data_blocks_per_stripe, 64);
        assert_eq!(s.parity_blocks_per_stripe, 6);
        assert_eq!(s.stripes_per_neighborhood, 256);
        // 256 × (64 + 6) = 17,920 blocks per neighborhood.
        assert_eq!(s.neighborhood_blocks(), 17_920);
        // 6/64 = 0.09375 = 9.375% overhead (design says 9.4%).
        assert!((s.overhead_ratio() - (6.0 / 64.0)).abs() < 1e-9);
        assert!((s.parity_over_data_ratio() - (6.0 / 64.0)).abs() < 1e-9);
        assert!((s.parity_fraction_of_total_written() - (6.0 / 70.0)).abs() < 1e-9);
        // 256 × 6 = 1,536 blocks at 256 KiB (~384 MiB).
        assert_eq!(s.contiguous_damage_threshold(), 1_536);
    }

    #[test]
    fn block_size_aware_schemes_preserve_loss_tolerance() {
        assert_eq!(
            default_scheme_for_block_size(256 * 1024).contiguous_damage_threshold(),
            2_048
        );
        assert_eq!(
            default_scheme_for_block_size(1024 * 1024).contiguous_damage_threshold(),
            512
        );
        assert_eq!(
            conservative_scheme_for_block_size(256 * 1024).contiguous_damage_threshold(),
            1_536
        );
        assert_eq!(
            conservative_scheme_for_block_size(1024 * 1024).contiguous_damage_threshold(),
            384
        );
    }

    #[test]
    fn validate_accepts_default_and_conservative() {
        default_scheme().validate().expect("default ok");
        conservative_scheme().validate().expect("conservative ok");
    }

    #[test]
    fn validate_rejects_k_below_two() {
        let s = ParityScheme {
            id: SchemeId::new_static("test"),
            data_blocks_per_stripe: 1,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 1,
        };
        let err = s.validate().unwrap_err();
        assert!(format!("{err}").contains("data_blocks_per_stripe"));
    }

    #[test]
    fn validate_rejects_m_zero() {
        let s = ParityScheme {
            id: SchemeId::new_static("test"),
            data_blocks_per_stripe: 4,
            parity_blocks_per_stripe: 0,
            stripes_per_neighborhood: 1,
        };
        let err = s.validate().unwrap_err();
        assert!(format!("{err}").contains("no-parity flag"));
    }

    #[test]
    fn validate_rejects_m_greater_than_k() {
        let s = ParityScheme {
            id: SchemeId::new_static("test"),
            data_blocks_per_stripe: 4,
            parity_blocks_per_stripe: 5,
            stripes_per_neighborhood: 1,
        };
        let err = s.validate().unwrap_err();
        assert!(format!("{err}").contains("use replication"));
    }

    #[test]
    fn validate_rejects_zero_stripes_per_neighborhood() {
        let s = ParityScheme {
            id: SchemeId::new_static("test"),
            data_blocks_per_stripe: 4,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 0,
        };
        let err = s.validate().unwrap_err();
        assert!(format!("{err}").contains("stripes_per_neighborhood"));
    }

    #[test]
    fn validate_rejects_stripe_width_above_gf256_limit() {
        // k + m = 256 > 255.
        let s = ParityScheme {
            id: SchemeId::new_static("test"),
            data_blocks_per_stripe: 250,
            parity_blocks_per_stripe: 6,
            stripes_per_neighborhood: 1,
        };
        let err = s.validate().unwrap_err();
        assert!(format!("{err}").contains("255"));
    }

    #[test]
    fn overhead_ratio_with_k_zero_returns_zero_not_nan() {
        // Invariant: even invalid schemes don't NaN here.
        let s = ParityScheme {
            id: SchemeId::new_static("nope"),
            data_blocks_per_stripe: 0,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 1,
        };
        assert_eq!(s.overhead_ratio(), 0.0);
    }

    #[test]
    fn neighborhood_blocks_u64_math_handles_unrealistic_inputs() {
        // The neighborhood_blocks() method itself uses u64 math
        // so it doesn't overflow even at silly inputs.
        // validate() rejects this — the assertion just pins
        // that the math doesn't panic / wrap.
        let s = ParityScheme {
            id: SchemeId::new_static("max"),
            data_blocks_per_stripe: 250,
            parity_blocks_per_stripe: 5,
            stripes_per_neighborhood: u32::MAX,
        };
        let n = s.neighborhood_blocks();
        assert_eq!(n, (u32::MAX as u64) * 255);
    }

    #[test]
    fn validate_rejects_neighborhood_blocks_above_u32_max() {
        // Codex idref=36acce12 Medium: design §11.3 requires
        // neighborhood total to fit in u32.
        let s = ParityScheme {
            id: SchemeId::new_static("oversized"),
            data_blocks_per_stripe: 250,
            parity_blocks_per_stripe: 5,
            stripes_per_neighborhood: u32::MAX,
        };
        let err = s.validate().unwrap_err();
        assert!(format!("{err}").contains("neighborhood_blocks"), "{err}");
    }

    #[test]
    fn validate_accepts_largest_neighborhood_fitting_in_u32() {
        // Boundary check: a scheme that yields neighborhood_blocks
        // == u32::MAX exactly should pass.
        // u32::MAX = 4_294_967_295 = 16_843_009 × 255.
        let s = ParityScheme {
            id: SchemeId::new_static("at-u32-max"),
            data_blocks_per_stripe: 250,
            parity_blocks_per_stripe: 5,
            stripes_per_neighborhood: 16_843_009,
        };
        assert_eq!(s.neighborhood_blocks(), u32::MAX as u64);
        s.validate().expect("at-u32-max should pass");
    }

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

        let overflow = range.includes_object(u64::MAX, 1).unwrap_err();
        assert!(format!("{overflow}").contains("ordinal range overflows"));
    }
}
