//! Format-independent admission reasons and immutable manual request identity.

use serde::{Deserialize, Serialize};

/// Durable trigger that permanently closes Object admission on a tape.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalFinalizationTrigger {
    /// A committed Object reached the configured low watermark.
    ReachedLowWatermark,
    /// The drive reported early warning at a committed boundary.
    HardwareEarlyWarning,
    /// An authenticated operator requested early close-out.
    OperatorCloseOut,
    /// An authenticated operator requested pool-wide close-out.
    PoolCloseOut,
    /// No queued Object can fit while preserving the terminal reserve.
    NoPendingObjectFits,
}

/// Durable operation and request identity for manual early finalization.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ManualTerminalFinalizationIdentity {
    /// Durable operation UUID.
    pub operation_id: [u8; 16],
    /// Stable Layer-5 operation kind in the idempotency scope.
    pub operation_kind: String,
    /// Authenticated actor fingerprint that scopes the idempotency key.
    pub actor_fingerprint: String,
    /// Caller-provided durable idempotency UUID.
    pub idempotency_key: [u8; 16],
    /// Hash of the exact state-changing request.
    pub request_fingerprint: [u8; 32],
    /// Assignment observed under the per-tape owner.
    pub assigned_pool_id: Option<String>,
    /// Wire presence/value supplied by the caller.
    pub expected_pool_id: Option<String>,
    /// Assignment generation observed under the per-tape owner.
    pub assignment_generation: u64,
    /// Exact operator reason bytes represented as UTF-8.
    pub reason: String,
}

impl TerminalFinalizationTrigger {
    /// Stable public display spelling of the typed barrier trigger.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HardwareEarlyWarning => "hardware_early_warning",
            Self::ReachedLowWatermark => "reached_low_watermark",
            Self::OperatorCloseOut => "operator_close_out",
            Self::PoolCloseOut => "pool_close_out",
            Self::NoPendingObjectFits => "no_pending_object_fits",
        }
    }
}
