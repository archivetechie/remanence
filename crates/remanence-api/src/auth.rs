//! Request-role extraction and Layer 5 permission checks.
//!
//! Certificate roles require an explicit `remanence-role` attribute. The
//! development metadata path remains separately supported for local clients.

use remanence_state::AuditActor;
use sha2::{Digest, Sha256};
use tonic::{Request, Status};

use crate::hex_encoding::hex_lower;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthPermission {
    Read,
    ReadTape,
    Write,
    Robotics,
    Lifecycle,
    OperationControl,
}

impl AuthPermission {
    fn label(self) -> &'static str {
        match self {
            Self::Read => "read-only RPCs",
            Self::ReadTape => "read-session RPCs",
            Self::Write => "write-session RPCs",
            Self::Robotics => "library robotics RPCs",
            Self::Lifecycle => "lifecycle RPCs",
            Self::OperationControl => "operation-control RPCs",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClientRole {
    System,
    Readonly,
    Operator,
    Orchestrator,
    Admin,
}

impl ClientRole {
    fn label(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Readonly => "readonly",
            Self::Operator => "operator",
            Self::Orchestrator => "orchestrator",
            Self::Admin => "admin",
        }
    }

    pub(super) fn allows(self, permission: AuthPermission) -> bool {
        match self {
            Self::System => true,
            Self::Admin | Self::Orchestrator => !matches!(permission, AuthPermission::Lifecycle),
            Self::Operator => !matches!(
                permission,
                AuthPermission::Write | AuthPermission::Lifecycle
            ),
            Self::Readonly => matches!(permission, AuthPermission::Read | AuthPermission::ReadTape),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AuthContext {
    actor: AuditActor,
    role: ClientRole,
}

pub(crate) fn actor_from_request<T>(request: &Request<T>) -> AuditActor {
    if let Some(certs) = request.peer_certs() {
        if let Some(cert) = certs.first() {
            return AuditActor::Service(format!(
                "mtls-cert-sha256:{}",
                hex_lower(&Sha256::digest(cert.as_ref()))
            ));
        }
    }

    request
        .metadata()
        .get("x-remanence-actor")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| AuditActor::Service(value.to_string()))
        .unwrap_or(AuditActor::System)
}

pub(crate) fn authorize_request<T>(
    request: &Request<T>,
    permission: AuthPermission,
) -> Result<AuditActor, Status> {
    let auth = auth_context_from_request(request)?;
    if auth.role.allows(permission) {
        Ok(auth.actor)
    } else {
        Err(Status::permission_denied(format!(
            "role {} is not authorized for {}",
            auth.role.label(),
            permission.label()
        )))
    }
}

fn auth_context_from_request<T>(request: &Request<T>) -> Result<AuthContext, Status> {
    let actor = actor_from_request(request);
    let role = if let Some(certs) = request.peer_certs() {
        certs
            .first()
            .and_then(|cert| role_from_certificate_subject(cert.as_ref()))
            .unwrap_or(ClientRole::Readonly)
    } else {
        role_from_metadata(request)?.unwrap_or(ClientRole::System)
    };
    Ok(AuthContext { actor, role })
}

fn role_from_metadata<T>(request: &Request<T>) -> Result<Option<ClientRole>, Status> {
    let Some(value) = request.metadata().get("x-remanence-role") else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| Status::permission_denied("x-remanence-role must be printable ASCII"))?;
    parse_client_role(value)
        .map(Some)
        .ok_or_else(|| Status::permission_denied("unrecognized x-remanence-role"))
}

pub(super) fn role_from_certificate_subject(cert_der: &[u8]) -> Option<ClientRole> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert_der).ok()?;
    for attr in cert.subject().iter_attributes() {
        if let Ok(value) = attr.as_str() {
            if let Some(role) = parse_certificate_role_attribute(value) {
                return Some(role);
            }
        }
    }
    None
}

/// Certificate subjects grant a role only through an explicit
/// `remanence-role=<role>` (or `remanence-role:<role>`) attribute
/// value. Bare role words are deliberately NOT honored here: subject
/// attributes routinely carry human-chosen names, and a certificate
/// whose CN happens to read "operator" or "admin" must not silently
/// receive that privilege. (The `x-remanence-role` metadata path keeps
/// accepting bare words — there the header name itself states intent.)
pub(super) fn parse_certificate_role_attribute(value: &str) -> Option<ClientRole> {
    let lower = value.trim().to_ascii_lowercase();
    let stripped = lower
        .strip_prefix("remanence-role=")
        .or_else(|| lower.strip_prefix("remanence-role:"))?;
    parse_role_word(stripped.trim())
}

pub(super) fn parse_client_role(value: &str) -> Option<ClientRole> {
    let lower = value.trim().to_ascii_lowercase();
    let mut value = lower.as_str();
    for prefix in ["remanence-role=", "remanence-role:", "role=", "role:"] {
        if let Some(stripped) = value.strip_prefix(prefix) {
            value = stripped.trim();
            break;
        }
    }
    parse_role_word(value)
}

fn parse_role_word(value: &str) -> Option<ClientRole> {
    match value {
        "system" => Some(ClientRole::System),
        "readonly" | "read-only" | "read_only" => Some(ClientRole::Readonly),
        "operator" => Some(ClientRole::Operator),
        "orchestrator" => Some(ClientRole::Orchestrator),
        "admin" => Some(ClientRole::Admin),
        _ => None,
    }
}
