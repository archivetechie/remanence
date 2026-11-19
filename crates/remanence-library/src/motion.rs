//! Authority callbacks at the actual changer/drive dispatch gates. The library
//! owns physical targets and completion evidence; its caller owns durable typed
//! publication. No callback runs while the shared inventory mutex is held.
use crate::{outcome::CompletionEvidence, DriveKey};

/// Exact CDB target, independent of a composer's admission placeholder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MotionCommand {
    /// One SSC LOAD.
    Load(DriveKey),
    /// One SSC UNLOAD.
    Unload(DriveKey),
    /// One changer MOVE MEDIUM.
    Move {
        /// Actual changer serial.
        library_serial: String,
        /// CDB source element.
        source: u16,
        /// CDB destination element.
        destination: u16,
    },
}
/// The composer responsible for closing a successful LOAD.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LoadConfirmation {
    /// No readiness flow follows; perform a physical inventory observation.
    #[default]
    Inventory,
    /// The stable readiness operation publishes ReadinessObserved then Done.
    Readiness,
}
/// Process-local proof naming the exact cleaning obligation and selected cartridge.
/// The runtime validates this against retained authority before admitting motion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaintenanceCapability {
    /// Active drive-fence raise owned by this cleaning operation.
    pub fence: u64,
    /// Cleaning run retaining the selected cartridge and home slot.
    pub run: String,
    /// UUID of the run's selected cleaning cartridge.
    pub cartridge: [u8; 16],
}
/// Immutable operation facts captured for one dispatch owner.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MotionContext {
    /// Stable readiness or operation identity, when this call has one.
    pub parent_operation: Option<[u8; 16]>,
    /// Known physical tape identity and incarnation, never inferred from a label.
    pub tape: Option<([u8; 16], u64)>,
    /// Inventory generation used by the composer.
    pub inventory_generation: Option<u64>,
    /// Readiness generation used by the composer.
    pub readiness_generation: Option<u64>,
    /// Explicit owner for a successful LOAD's final confirmation.
    pub load_confirmation: LoadConfirmation,
    /// Validated only at dispatch; never written into MotionIntent.
    pub maintenance: Option<MaintenanceCapability>,
}
/// One successful command awaiting its owner's physical confirmation.
#[derive(Clone, Debug)]
pub struct PendingMotion {
    /// Exact physical command target.
    pub command: MotionCommand,
    /// Facts captured before dispatch.
    pub context: MotionContext,
    /// Durable intent acknowledgement.
    pub ticket: MotionTicket,
    /// Captured successful CDB completion, before any observation commands.
    pub evidence: CompletionEvidence,
}
/// Durable source reference returned only after the intent is acknowledged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MotionTicket {
    /// Source sequence in the library authority.
    pub sequence: u64,
    /// Source record UUID.
    pub record_uuid: [u8; 16],
}
/// Proven failure class controls whether a continuation LOAD may return media.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MotionAuthorityFailure {
    /// Healthy writer refused this candidate before publication.
    SemanticRefusal,
    /// Authority storage failed or remains fenced; an intent may have landed.
    StoreFailure,
    /// Durable publication may exist but projection delivery failed.
    ProjectionFailure,
    /// Native evidence or another authority prerequisite was unavailable.
    EvidenceFailure,
    /// Existing open/Unknown motion requires a physical scope reconciliation.
    UnresolvedMotion,
}
/// Preserve storage failure separately from a healthy semantic refusal.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct MotionAuthorityError {
    /// Human-readable refusal or persistence error.
    pub message: String,
    /// Only SemanticRefusal permits compensating motion.
    pub kind: MotionAuthorityFailure,
}
/// Immutable filesystem binding of the runtime owning a motion handle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MotionAuthorityIdentity {
    /// Bound library log identity.
    pub library_uuid: [u8; 16],
    /// Canonical library-log directory.
    pub library_root: std::path::PathBuf,
    /// Canonical native-log directory.
    pub tape_root: std::path::PathBuf,
}
/// Shared authority installed by a production runtime before motion admission.
pub trait MotionAuthority: Send + Sync {
    /// Concrete runtime binding; test authorities may omit it.
    fn identity(&self) -> Option<MotionAuthorityIdentity> {
        None
    }
    /// Whether startup must observe this physical scope before another command.
    fn requires_reconciliation(&self, _command: &MotionCommand) -> bool {
        false
    }
    /// Whether this library has unresolved motion with no live owner in this process.
    fn unowned_motion_unresolved(&self, _library_serial: &str) -> bool {
        false
    }
    /// Resolve only this changer's open/Unknown intents after a fresh full RES.
    /// Intents still owned by this runtime instance are left open.
    fn reconcile_inventory(&self, _observed: &crate::Library) -> Result<(), MotionAuthorityError> {
        Err(MotionAuthorityError {
            message: "startup inventory resolver is unavailable".into(),
            kind: MotionAuthorityFailure::EvidenceFailure,
        })
    }
    /// Resolve only this standalone endpoint after matching TUR/READ POSITION.
    /// Intents still owned by this runtime instance are left open.
    fn reconcile_standalone(&self, _endpoint: &DriveKey) -> Result<(), MotionAuthorityError> {
        Err(MotionAuthorityError {
            message: "startup standalone resolver is unavailable".into(),
            kind: MotionAuthorityFailure::EvidenceFailure,
        })
    }
    /// Append and project the exact per-CDB intent. A failure sends no CDB.
    fn intent(
        &self,
        command: &MotionCommand,
        context: &MotionContext,
    ) -> Result<MotionTicket, MotionAuthorityError>;
    /// Persist Failed or Unknown from the captured completion. Successful
    /// commands remain owned by the composer until physical confirmation.
    fn failed(
        &self,
        ticket: MotionTicket,
        evidence: &CompletionEvidence,
    ) -> Result<(), MotionAuthorityError>;
    /// The CDB completed but physical confirmation could not be obtained.
    /// Persist Unknown with the original CDB evidence; never replay the CDB.
    fn unconfirmed(&self, pending: &PendingMotion) -> Result<(), MotionAuthorityError>;
    /// Publish a fresh physical changer observation without resolving the intent,
    /// then close this command with Done(Inventory).
    fn confirm_inventory(
        &self,
        pending: &PendingMotion,
        observed: &crate::Library,
    ) -> Result<(), MotionAuthorityError>;
    /// TUR and READ POSITION have independently proved this standalone endpoint.
    fn confirm_standalone(&self, pending: &PendingMotion) -> Result<(), MotionAuthorityError>;
    /// A readiness owner has actually observed ready after its LOAD.
    fn confirm_readiness(
        &self,
        pending: &PendingMotion,
        observed: &crate::MediaReadiness,
        limit: std::time::Duration,
    ) -> Result<(), MotionAuthorityError>;
    /// Refuse every later motion after a publication failure, including returns.
    fn ensure_motion_allowed(&self) -> Result<(), MotionAuthorityError>;
}
