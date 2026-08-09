//! Daemon-backed `rem tape finalize` command.
//!
//! The command validates the irreversible target and acknowledgement before
//! opening a daemon connection, submits exactly one idempotent finalization
//! request, and optionally polls the read-only status RPC across reconnects.

use std::collections::BTreeSet;
use std::io::Write;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use clap::Args;
use remanence_api::pb;
use serde_json::{json, Value};
use tonic::transport::Channel;
use uuid::Uuid;

use super::{
    bytes_to_hex, bytes_to_uuid_text, connect_daemon, daemon_runtime, finish_daemon_client_result,
    print_json_envelope, DaemonClientError, DEFAULT_DAEMON_ENDPOINT,
};

const FINALIZATION_POLL_INTERVAL: Duration = Duration::from_secs(1);
const FINALIZATION_JSON_SCHEMA: &str = "rem.tape.finalization.v1";

#[derive(Args, Clone, Debug)]
pub(crate) struct TapeFinalizeArgs {
    /// Exact Remanence tape UUID. Voltags are not accepted.
    #[arg(long, value_name = "UUID")]
    pub(crate) tape_uuid: String,

    /// Current canonical pool id. Required by the daemon for a pooled tape.
    #[arg(long, value_name = "ID")]
    pub(crate) expected_pool: Option<String>,

    /// Exact non-empty operator reason recorded by the daemon.
    #[arg(long, value_name = "EXACT")]
    pub(crate) reason: String,

    /// Required durable UUID key for retry deduplication.
    #[arg(long, value_name = "UUID")]
    pub(crate) idempotency_key: String,

    /// Poll finalization status until the daemon reports a terminal outcome.
    #[arg(long)]
    pub(crate) wait: bool,

    /// Emit stable CLI-shaped JSON.
    #[arg(long)]
    pub(crate) json: bool,

    /// Explicit acknowledgement of the exact irreversible tape UUID.
    #[arg(long, value_name = "UUID")]
    pub(crate) ack_tape_uuid: String,

    /// Daemon gRPC endpoint URI.
    #[arg(long, value_name = "URI", default_value = DEFAULT_DAEMON_ENDPOINT)]
    pub(crate) endpoint: String,
}

#[derive(Debug)]
struct ValidatedFinalize {
    request: pb::FinalizeTapeRequest,
    tape_uuid: [u8; 16],
}

impl TapeFinalizeArgs {
    fn validate(&self) -> Result<ValidatedFinalize, String> {
        let tape_uuid = parse_exact_uuid(&self.tape_uuid, "tape_uuid")?;
        let acknowledged = parse_exact_uuid(&self.ack_tape_uuid, "ack_tape_uuid")?;
        if acknowledged != tape_uuid {
            return Err(format!(
                "--ack-tape-uuid must exactly match --tape-uuid {}",
                Uuid::from_bytes(tape_uuid)
            ));
        }

        let expected_pool_id = match self.expected_pool.as_deref() {
            None => None,
            Some(value) if value.trim().is_empty() => {
                return Err("--expected-pool cannot be empty or whitespace-only".to_string())
            }
            Some(value) if value.trim() != value => {
                return Err(
                    "--expected-pool must not have leading or trailing whitespace".to_string(),
                )
            }
            Some(value) => Some(value.to_string()),
        };
        if self.reason.trim().is_empty() {
            return Err("--reason cannot be empty or whitespace-only".to_string());
        }
        let idempotency_key = parse_exact_uuid(&self.idempotency_key, "idempotency_key")?;
        if idempotency_key == [0; 16] {
            return Err("--idempotency-key must not be the nil UUID".to_string());
        }

        Ok(ValidatedFinalize {
            request: pb::FinalizeTapeRequest {
                tape_uuid: tape_uuid.to_vec(),
                expected_pool_id,
                // Deliberately do not trim: these exact UTF-8 bytes are part of
                // the daemon's durable request fingerprint and audit record.
                reason: self.reason.clone(),
                idempotency_key: Some(pb::IdempotencyKey {
                    value: idempotency_key.to_vec(),
                }),
            },
            tape_uuid,
        })
    }
}

fn parse_exact_uuid(value: &str, field: &str) -> Result<[u8; 16], String> {
    Uuid::parse_str(value)
        .map(|uuid| *uuid.as_bytes())
        .map_err(|error| format!("invalid {field} {value:?}: {error}"))
}

trait FinalizationTransport {
    fn finalize(
        &mut self,
        request: pb::FinalizeTapeRequest,
    ) -> Result<pb::TapeFinalization, DaemonClientError>;

    fn get(&mut self, tape_uuid: Vec<u8>) -> Result<pb::TapeFinalization, PollFinalizationError>;
}

#[derive(Debug)]
enum PollFinalizationError {
    Retryable,
    Fatal(DaemonClientError),
}

struct DaemonFinalizationTransport {
    endpoint: String,
    runtime: tokio::runtime::Runtime,
    client: Option<pb::catalog_client::CatalogClient<Channel>>,
}

impl DaemonFinalizationTransport {
    fn new(endpoint: &str) -> Result<Self, DaemonClientError> {
        Ok(Self {
            endpoint: endpoint.to_string(),
            runtime: daemon_runtime()?,
            client: None,
        })
    }

    fn connect(&mut self) -> Result<(), String> {
        if self.client.is_none() {
            let channel = self.runtime.block_on(connect_daemon(&self.endpoint))?;
            self.client = Some(pb::catalog_client::CatalogClient::new(channel));
        }
        Ok(())
    }
}

impl FinalizationTransport for DaemonFinalizationTransport {
    fn finalize(
        &mut self,
        request: pb::FinalizeTapeRequest,
    ) -> Result<pb::TapeFinalization, DaemonClientError> {
        self.connect().map_err(DaemonClientError::from)?;
        let response = self.runtime.block_on(
            self.client
                .as_mut()
                .expect("connected finalization client")
                .finalize_tape(request),
        );
        response
            .map(tonic::Response::into_inner)
            .map_err(DaemonClientError::status)
    }

    fn get(&mut self, tape_uuid: Vec<u8>) -> Result<pb::TapeFinalization, PollFinalizationError> {
        if self.connect().is_err() {
            self.client = None;
            return Err(PollFinalizationError::Retryable);
        }
        let response = self.runtime.block_on(
            self.client
                .as_mut()
                .expect("connected finalization client")
                .get_tape_finalization(pb::GetTapeFinalizationRequest { tape_uuid }),
        );
        match response {
            Ok(response) => Ok(response.into_inner()),
            Err(error) if is_retryable_poll_status(error.code()) => {
                // Force a fresh connector on the next read-only poll. The
                // state-changing FinalizeTape RPC is never replayed here.
                self.client = None;
                Err(PollFinalizationError::Retryable)
            }
            Err(error) => Err(PollFinalizationError::Fatal(DaemonClientError::status(
                error,
            ))),
        }
    }
}

fn is_retryable_poll_status(code: tonic::Code) -> bool {
    matches!(
        code,
        tonic::Code::Unavailable | tonic::Code::Cancelled | tonic::Code::DeadlineExceeded
    )
}

pub(crate) fn run(args: &TapeFinalizeArgs, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let validated = match args.validate() {
        Ok(validated) => validated,
        Err(error) => {
            return finish_daemon_client_result(
                Err(DaemonClientError::client(error)),
                args.json,
                err,
            )
        }
    };
    let mut transport = match DaemonFinalizationTransport::new(&args.endpoint) {
        Ok(transport) => transport,
        Err(error) => return finish_daemon_client_result(Err(error), args.json, err),
    };
    execute_command_with_transport(
        &mut transport,
        validated,
        args.wait,
        args.json,
        FINALIZATION_POLL_INTERVAL,
        out,
        err,
    )
}

/// Execute the complete command lifecycle against an injected transport so
/// rendering and process status are tested together with RPC semantics.
fn execute_command_with_transport<T: FinalizationTransport>(
    transport: &mut T,
    validated: ValidatedFinalize,
    wait: bool,
    json_output: bool,
    poll_interval: Duration,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let finalization = match execute_with_transport(transport, validated, wait, poll_interval) {
        Ok(finalization) => finalization,
        Err(error) => return finish_daemon_client_result(Err(error), json_output, err),
    };

    finish_finalization_command(&finalization, json_output, out, err)
}

/// Render one validated daemon result and convert its terminal outcome into
/// the command's process exit status.
fn finish_finalization_command(
    finalization: &pb::TapeFinalization,
    json_output: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    if let Err(error) = print_finalization(finalization, json_output, out) {
        return finish_daemon_client_result(
            Err(DaemonClientError::client(error)),
            json_output,
            err,
        );
    }
    if is_unsuccessful_terminal(finalization.outcome) {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn execute_with_transport<T: FinalizationTransport>(
    transport: &mut T,
    validated: ValidatedFinalize,
    wait: bool,
    poll_interval: Duration,
) -> Result<pb::TapeFinalization, DaemonClientError> {
    let mut status = transport.finalize(validated.request)?;
    let operation_id = validate_status(&status, &validated.tape_uuid, None)?;
    let terminal = is_terminal(status.outcome)?;
    if !wait || terminal {
        return Ok(status);
    }
    let operation_id = operation_id.expect("validated in-progress finalization has operation_id");

    loop {
        if !poll_interval.is_zero() {
            thread::sleep(poll_interval);
        }
        status = match transport.get(validated.tape_uuid.to_vec()) {
            Ok(status) => status,
            Err(PollFinalizationError::Retryable) => continue,
            Err(PollFinalizationError::Fatal(error)) => return Err(error),
        };
        validate_status(&status, &validated.tape_uuid, Some(&operation_id))?;
        if is_terminal(status.outcome)? {
            return Ok(status);
        }
    }
}

fn validate_status(
    status: &pb::TapeFinalization,
    expected_tape_uuid: &[u8; 16],
    expected_operation_id: Option<&[u8; 16]>,
) -> Result<Option<[u8; 16]>, DaemonClientError> {
    let tape_uuid = <[u8; 16]>::try_from(status.tape_uuid.as_slice()).map_err(|_| {
        DaemonClientError::client(format!(
            "daemon returned tape_uuid with {} bytes; expected 16",
            status.tape_uuid.len()
        ))
    })?;
    if &tape_uuid != expected_tape_uuid {
        return Err(DaemonClientError::client(format!(
            "daemon returned finalization status for tape {}, expected {}",
            Uuid::from_bytes(tape_uuid),
            Uuid::from_bytes(*expected_tape_uuid)
        )));
    }
    // BUSY is an admission result, not an accepted operation. The daemon has
    // made no durable transition or media motion, so accepted-operation fields
    // are deliberately absent.
    if matches!(
        pb::TapeFinalizationOutcome::try_from(status.outcome),
        Ok(pb::TapeFinalizationOutcome::Busy)
    ) {
        if expected_operation_id.is_some() {
            return Err(DaemonClientError::client(
                "daemon returned BUSY while polling an accepted finalization operation",
            ));
        }
        if !status.operation_id.is_empty()
            || status.progress != pb::TapeFinalizationProgress::Unspecified as i32
            || status.completed_replicas != 0
            || !status.replica_health.is_empty()
            || !status.edition_digest.is_empty()
            || !status.layout_digest.is_empty()
        {
            return Err(DaemonClientError::client(
                "daemon returned BUSY with accepted-operation fields",
            ));
        }
        return Ok(None);
    }
    // Reject an unspecified or unknown outcome before validating the fields
    // required on every accepted operation.
    let _ = is_terminal(status.outcome)?;
    let operation_id = if status.operation_id.is_empty() {
        return Err(DaemonClientError::client(
            "daemon returned finalization status without an operation_id",
        ));
    } else {
        let operation_id = <[u8; 16]>::try_from(status.operation_id.as_slice()).map_err(|_| {
            DaemonClientError::client(format!(
                "daemon returned operation_id with {} bytes; expected 16",
                status.operation_id.len()
            ))
        })?;
        if operation_id == [0; 16] {
            return Err(DaemonClientError::client(
                "daemon returned the nil finalization operation_id",
            ));
        }
        Some(operation_id)
    };
    if expected_operation_id.is_some_and(|expected| operation_id.as_ref() != Some(expected)) {
        return Err(DaemonClientError::client(
            "daemon changed the finalization operation_id while polling",
        ));
    }
    if status.completed_replicas > 3 {
        return Err(DaemonClientError::client(format!(
            "daemon returned invalid completed_replicas {}",
            status.completed_replicas
        )));
    }
    let mut ordinals = BTreeSet::new();
    for replica in &status.replica_health {
        if !(1..=3).contains(&replica.replica_ordinal) {
            return Err(DaemonClientError::client(format!(
                "daemon returned invalid replica ordinal {}",
                replica.replica_ordinal
            )));
        }
        if !ordinals.insert(replica.replica_ordinal) {
            return Err(DaemonClientError::client(format!(
                "daemon returned duplicate replica ordinal {}",
                replica.replica_ordinal
            )));
        }
    }
    if ordinals.len() != 3 {
        return Err(DaemonClientError::client(format!(
            "daemon returned {} replica-health rows; expected ordinals 1, 2, and 3",
            ordinals.len()
        )));
    }
    for (name, digest) in [
        ("edition_digest", status.edition_digest.as_slice()),
        ("layout_digest", status.layout_digest.as_slice()),
    ] {
        if !digest.is_empty() && digest.len() != 32 {
            return Err(DaemonClientError::client(format!(
                "daemon returned {name} with {} bytes; expected 32 or absent",
                digest.len()
            )));
        }
    }
    Ok(operation_id)
}

fn is_terminal(outcome: i32) -> Result<bool, DaemonClientError> {
    match pb::TapeFinalizationOutcome::try_from(outcome) {
        Ok(pb::TapeFinalizationOutcome::Finalizing) => Ok(false),
        Ok(pb::TapeFinalizationOutcome::Finalized)
        | Ok(pb::TapeFinalizationOutcome::FinalizedDegraded)
        | Ok(pb::TapeFinalizationOutcome::RecoveryRequired)
        | Ok(pb::TapeFinalizationOutcome::Failed)
        | Ok(pb::TapeFinalizationOutcome::Busy) => Ok(true),
        Ok(pb::TapeFinalizationOutcome::Unspecified) => Err(DaemonClientError::client(
            "daemon returned unspecified tape finalization outcome",
        )),
        Err(_) => Err(DaemonClientError::client(format!(
            "daemon returned unknown tape finalization outcome {outcome}"
        ))),
    }
}

fn is_unsuccessful_terminal(outcome: i32) -> bool {
    matches!(
        pb::TapeFinalizationOutcome::try_from(outcome),
        Ok(pb::TapeFinalizationOutcome::Busy
            | pb::TapeFinalizationOutcome::RecoveryRequired
            | pb::TapeFinalizationOutcome::Failed)
    )
}

fn print_finalization(
    finalization: &pb::TapeFinalization,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    if json_output {
        return print_json_envelope(
            FINALIZATION_JSON_SCHEMA,
            "item",
            finalization_json(finalization)?,
            out,
        );
    }

    writeln!(
        out,
        "tape_uuid: {}",
        bytes_to_uuid_text(&finalization.tape_uuid)
    )
    .map_err(|error| format!("write finalization status: {error}"))?;
    let operation_id = if finalization.operation_id.is_empty() {
        "-".to_string()
    } else {
        bytes_to_uuid_text(&finalization.operation_id)
    };
    writeln!(out, "operation_id: {operation_id}")
        .map_err(|error| format!("write finalization status: {error}"))?;
    writeln!(out, "outcome: {}", outcome_name(finalization.outcome)?)
        .map_err(|error| format!("write finalization status: {error}"))?;
    writeln!(out, "trigger: {}", finalization.trigger)
        .map_err(|error| format!("write finalization status: {error}"))?;
    writeln!(out, "progress: {}", progress_name(finalization.progress))
        .map_err(|error| format!("write finalization status: {error}"))?;
    writeln!(
        out,
        "completed_replicas: {}/3",
        finalization.completed_replicas
    )
    .map_err(|error| format!("write finalization status: {error}"))?;
    writeln!(
        out,
        "operator_recovery_required: {}",
        matches!(
            pb::TapeFinalizationOutcome::try_from(finalization.outcome),
            Ok(pb::TapeFinalizationOutcome::RecoveryRequired)
        )
    )
    .map_err(|error| format!("write finalization status: {error}"))?;
    if !finalization.replica_health.is_empty() {
        writeln!(out, "replica_health:")
            .map_err(|error| format!("write finalization status: {error}"))?;
        for replica in ordered_replica_health(finalization) {
            let detail = if replica.detail.is_empty() {
                String::new()
            } else {
                format!(" ({})", replica.detail)
            };
            writeln!(
                out,
                "  replica {}: {}{}",
                replica.replica_ordinal,
                replica_state_name(replica.state),
                detail
            )
            .map_err(|error| format!("write finalization status: {error}"))?;
        }
    }
    if !finalization.edition_digest.is_empty() {
        writeln!(
            out,
            "edition_digest: {}",
            bytes_to_hex(&finalization.edition_digest)
        )
        .map_err(|error| format!("write finalization status: {error}"))?;
    }
    if !finalization.layout_digest.is_empty() {
        writeln!(
            out,
            "layout_digest: {}",
            bytes_to_hex(&finalization.layout_digest)
        )
        .map_err(|error| format!("write finalization status: {error}"))?;
    }
    if !finalization.detail.is_empty() {
        writeln!(out, "detail: {}", finalization.detail)
            .map_err(|error| format!("write finalization status: {error}"))?;
    }
    Ok(())
}

fn finalization_json(finalization: &pb::TapeFinalization) -> Result<Value, String> {
    let terminal = is_terminal(finalization.outcome).map_err(|error| error.message)?;
    let replica_health = ordered_replica_health(finalization)
        .into_iter()
        .map(|replica| {
            json!({
                "replica_ordinal": replica.replica_ordinal,
                "state": replica_state_name(replica.state),
                "detail": replica.detail,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "tape_uuid": bytes_to_uuid_text(&finalization.tape_uuid),
        "operation_id": if finalization.operation_id.is_empty() {
            Value::Null
        } else {
            Value::String(bytes_to_uuid_text(&finalization.operation_id))
        },
        "outcome": outcome_name(finalization.outcome)?,
        "trigger": finalization.trigger,
        "progress": progress_name(finalization.progress),
        "completed_replicas": finalization.completed_replicas,
        "replica_count": 3,
        "replica_health": replica_health,
        "edition_digest": digest_json(&finalization.edition_digest),
        "layout_digest": digest_json(&finalization.layout_digest),
        "terminal": terminal,
        "operator_recovery_required": matches!(
            pb::TapeFinalizationOutcome::try_from(finalization.outcome),
            Ok(pb::TapeFinalizationOutcome::RecoveryRequired)
        ),
        "detail": finalization.detail,
    }))
}

fn ordered_replica_health(finalization: &pb::TapeFinalization) -> Vec<&pb::TapeIndexReplicaHealth> {
    let mut replicas = finalization.replica_health.iter().collect::<Vec<_>>();
    replicas.sort_unstable_by_key(|replica| replica.replica_ordinal);
    replicas
}

fn digest_json(bytes: &[u8]) -> Value {
    if bytes.is_empty() {
        Value::Null
    } else {
        Value::String(bytes_to_hex(bytes))
    }
}

fn progress_name(progress: i32) -> &'static str {
    match pb::TapeFinalizationProgress::try_from(progress) {
        Ok(pb::TapeFinalizationProgress::BeforeReplicaA) => "before_replica_a",
        Ok(pb::TapeFinalizationProgress::AfterReplicaA) => "after_replica_a",
        Ok(pb::TapeFinalizationProgress::AfterSeparationAb) => "after_separation_ab",
        Ok(pb::TapeFinalizationProgress::AfterReplicaB) => "after_replica_b",
        Ok(pb::TapeFinalizationProgress::AfterSeparationBc) => "after_separation_bc",
        Ok(pb::TapeFinalizationProgress::AfterReplicaC) => "after_replica_c",
        Ok(pb::TapeFinalizationProgress::Unspecified) => "unspecified",
        Err(_) => "unknown",
    }
}

fn outcome_name(outcome: i32) -> Result<&'static str, String> {
    match pb::TapeFinalizationOutcome::try_from(outcome) {
        Ok(pb::TapeFinalizationOutcome::Finalizing) => Ok("finalizing"),
        Ok(pb::TapeFinalizationOutcome::Finalized) => Ok("finalized"),
        Ok(pb::TapeFinalizationOutcome::FinalizedDegraded) => Ok("finalized_degraded"),
        Ok(pb::TapeFinalizationOutcome::RecoveryRequired) => Ok("recovery_required"),
        Ok(pb::TapeFinalizationOutcome::Failed) => Ok("failed"),
        Ok(pb::TapeFinalizationOutcome::Busy) => Ok("busy"),
        Ok(pb::TapeFinalizationOutcome::Unspecified) => {
            Err("daemon returned unspecified tape finalization outcome".to_string())
        }
        Err(_) => Err(format!(
            "daemon returned unknown tape finalization outcome {outcome}"
        )),
    }
}

fn replica_state_name(state: i32) -> &'static str {
    match pb::tape_index_replica_health::State::try_from(state) {
        Ok(pb::tape_index_replica_health::State::TapeIndexReplicaStatePending) => "pending",
        Ok(pb::tape_index_replica_health::State::TapeIndexReplicaStateComplete) => "complete",
        Ok(pb::tape_index_replica_health::State::TapeIndexReplicaStateEnvelopeValid) => {
            "envelope_valid"
        }
        Ok(pb::tape_index_replica_health::State::TapeIndexReplicaStateInvalid) => "invalid",
        Ok(pb::tape_index_replica_health::State::TapeIndexReplicaStateUnknown) => "unknown",
        Ok(pb::tape_index_replica_health::State::TapeIndexReplicaStateUnspecified) => "unspecified",
        Err(_) => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    fn args() -> TapeFinalizeArgs {
        TapeFinalizeArgs {
            tape_uuid: Uuid::from_u128(1).to_string(),
            expected_pool: Some("slow-offsite".to_string()),
            reason: "  ship this tape now  ".to_string(),
            idempotency_key: Uuid::from_u128(2).to_string(),
            wait: false,
            json: false,
            ack_tape_uuid: Uuid::from_u128(1).to_string(),
            endpoint: DEFAULT_DAEMON_ENDPOINT.to_string(),
        }
    }

    fn status(outcome: pb::TapeFinalizationOutcome) -> pb::TapeFinalization {
        pb::TapeFinalization {
            tape_uuid: Uuid::from_u128(1).as_bytes().to_vec(),
            operation_id: Uuid::from_u128(3).as_bytes().to_vec(),
            progress: pb::TapeFinalizationProgress::BeforeReplicaA as i32,
            completed_replicas: 0,
            replica_health: (1..=3)
                .map(|replica_ordinal| pb::TapeIndexReplicaHealth {
                    replica_ordinal,
                    state: pb::tape_index_replica_health::State::TapeIndexReplicaStatePending
                        as i32,
                    detail: String::new(),
                })
                .collect(),
            edition_digest: Vec::new(),
            layout_digest: Vec::new(),
            outcome: outcome as i32,
            trigger: "operator_close_out".to_string(),
            detail: "accepted".to_string(),
        }
    }

    fn busy_status() -> pb::TapeFinalization {
        pb::TapeFinalization {
            tape_uuid: Uuid::from_u128(1).as_bytes().to_vec(),
            operation_id: Vec::new(),
            progress: pb::TapeFinalizationProgress::Unspecified as i32,
            completed_replicas: 0,
            replica_health: Vec::new(),
            edition_digest: Vec::new(),
            layout_digest: Vec::new(),
            outcome: pb::TapeFinalizationOutcome::Busy as i32,
            trigger: "operator_close_out".to_string(),
            detail: "tape has an in-flight owner; no state or media motion occurred".to_string(),
        }
    }

    #[derive(Default)]
    struct MockTransport {
        finalize_calls: Vec<pb::FinalizeTapeRequest>,
        get_calls: Vec<Vec<u8>>,
        finalize_response: Option<pb::TapeFinalization>,
        polls: VecDeque<Result<pb::TapeFinalization, PollFinalizationError>>,
    }

    impl FinalizationTransport for MockTransport {
        fn finalize(
            &mut self,
            request: pb::FinalizeTapeRequest,
        ) -> Result<pb::TapeFinalization, DaemonClientError> {
            self.finalize_calls.push(request);
            Ok(self.finalize_response.take().expect("finalize response"))
        }

        fn get(
            &mut self,
            tape_uuid: Vec<u8>,
        ) -> Result<pb::TapeFinalization, PollFinalizationError> {
            self.get_calls.push(tape_uuid);
            self.polls.pop_front().expect("poll response")
        }
    }

    #[test]
    fn validation_preserves_exact_reason_and_presence() {
        let validated = args().validate().unwrap();
        assert_eq!(
            validated.request.reason.as_bytes(),
            b"  ship this tape now  "
        );
        assert_eq!(
            validated.request.expected_pool_id.as_deref(),
            Some("slow-offsite")
        );
        assert_eq!(validated.request.tape_uuid, Uuid::from_u128(1).as_bytes());
        assert_eq!(
            validated
                .request
                .idempotency_key
                .as_ref()
                .map(|key| key.value.as_slice()),
            Some(Uuid::from_u128(2).as_bytes().as_slice())
        );

        let mut unpooled = args();
        unpooled.expected_pool = None;
        assert_eq!(unpooled.validate().unwrap().request.expected_pool_id, None);
    }

    #[test]
    fn validation_rejects_non_uuid_target_and_mismatched_ack() {
        let mut invalid = args();
        invalid.tape_uuid = "RMI101L9".to_string();
        assert!(invalid
            .validate()
            .unwrap_err()
            .contains("invalid tape_uuid"));

        let mut mismatch = args();
        mismatch.ack_tape_uuid = Uuid::from_u128(9).to_string();
        assert!(mismatch
            .validate()
            .unwrap_err()
            .contains("must exactly match"));
    }

    #[test]
    fn validation_rejects_noncanonical_pool_and_blank_reason() {
        for pool in ["", "  ", " pool", "pool "] {
            let mut invalid = args();
            invalid.expected_pool = Some(pool.to_string());
            assert!(invalid.validate().is_err(), "pool {pool:?} must fail");
        }
        let mut invalid = args();
        invalid.reason = "\t \n".to_string();
        assert!(invalid.validate().unwrap_err().contains("--reason"));

        let mut invalid = args();
        invalid.idempotency_key = Uuid::nil().to_string();
        assert!(invalid
            .validate()
            .unwrap_err()
            .contains("must not be the nil UUID"));
    }

    #[test]
    fn no_wait_submits_once_and_does_not_poll() {
        let mut transport = MockTransport {
            finalize_response: Some(status(pb::TapeFinalizationOutcome::Finalizing)),
            ..MockTransport::default()
        };
        let response = execute_with_transport(
            &mut transport,
            args().validate().unwrap(),
            false,
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(
            response.outcome,
            pb::TapeFinalizationOutcome::Finalizing as i32
        );
        assert_eq!(transport.finalize_calls.len(), 1);
        assert!(transport.get_calls.is_empty());
    }

    #[test]
    fn finalize_response_requires_a_durable_operation_id_without_wait() {
        let mut response = status(pb::TapeFinalizationOutcome::Finalizing);
        response.operation_id.clear();
        let mut transport = MockTransport {
            finalize_response: Some(response),
            ..MockTransport::default()
        };
        let error = execute_with_transport(
            &mut transport,
            args().validate().unwrap(),
            false,
            Duration::ZERO,
        )
        .unwrap_err();
        assert!(error.message.contains("without an operation_id"));
        assert_eq!(transport.finalize_calls.len(), 1);
        assert!(transport.get_calls.is_empty());
    }

    #[test]
    fn busy_response_without_operation_fields_is_rendered_without_polling() {
        let mut transport = MockTransport {
            finalize_response: Some(busy_status()),
            ..MockTransport::default()
        };
        let response = execute_with_transport(
            &mut transport,
            args().validate().unwrap(),
            true,
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(response.outcome, pb::TapeFinalizationOutcome::Busy as i32);
        assert_eq!(transport.finalize_calls.len(), 1);
        assert!(transport.get_calls.is_empty());

        let mut out = Vec::new();
        print_finalization(&response, true, &mut out).unwrap();
        let value: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["data"]["outcome"], "busy");
        assert_eq!(value["data"]["operation_id"], Value::Null);
        assert_eq!(value["data"]["progress"], "unspecified");
        assert_eq!(value["data"]["completed_replicas"], 0);
        assert_eq!(value["data"]["replica_health"], json!([]));
        assert_eq!(value["data"]["terminal"], true);

        let mut out = Vec::new();
        print_finalization(&response, false, &mut out).unwrap();
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains("operation_id: -\n"));
        assert!(rendered.contains("outcome: busy\n"));
        assert!(rendered.contains("progress: unspecified\n"));
        assert!(!rendered.contains("replica_health:\n"));
    }

    #[test]
    fn busy_human_command_output_exits_nonzero() {
        let mut transport = MockTransport {
            finalize_response: Some(busy_status()),
            ..MockTransport::default()
        };
        let mut out = Vec::new();
        let mut err = Vec::new();

        let code = execute_command_with_transport(
            &mut transport,
            args().validate().unwrap(),
            true,
            false,
            Duration::ZERO,
            &mut out,
            &mut err,
        );

        assert_eq!(code, ExitCode::from(1));
        assert!(err.is_empty());
        assert_eq!(transport.finalize_calls.len(), 1);
        assert!(transport.get_calls.is_empty());
        let rendered = String::from_utf8(out).expect("human output is UTF-8");
        assert!(rendered.contains("outcome: busy\n"));
    }

    #[test]
    fn busy_json_command_output_exits_nonzero() {
        let mut transport = MockTransport {
            finalize_response: Some(busy_status()),
            ..MockTransport::default()
        };
        let mut out = Vec::new();
        let mut err = Vec::new();

        let code = execute_command_with_transport(
            &mut transport,
            args().validate().unwrap(),
            true,
            true,
            Duration::ZERO,
            &mut out,
            &mut err,
        );

        assert_eq!(code, ExitCode::from(1));
        assert!(err.is_empty());
        assert_eq!(transport.finalize_calls.len(), 1);
        assert!(transport.get_calls.is_empty());
        let value: Value = serde_json::from_slice(&out).expect("JSON command output");
        assert_eq!(value["data"]["outcome"], "busy");
    }

    #[test]
    fn busy_response_rejects_accepted_operation_fields() {
        let expected_tape = *Uuid::from_u128(1).as_bytes();
        let mut invalid = busy_status();
        invalid.operation_id = Uuid::from_u128(3).as_bytes().to_vec();
        assert!(validate_status(&invalid, &expected_tape, None)
            .unwrap_err()
            .message
            .contains("BUSY with accepted-operation fields"));

        let mut invalid = busy_status();
        invalid.replica_health = status(pb::TapeFinalizationOutcome::Finalizing).replica_health;
        assert!(validate_status(&invalid, &expected_tape, None)
            .unwrap_err()
            .message
            .contains("BUSY with accepted-operation fields"));
    }

    #[test]
    fn wait_polls_across_retryable_disconnect_without_resubmitting() {
        let mut final_status = status(pb::TapeFinalizationOutcome::Finalized);
        final_status.progress = pb::TapeFinalizationProgress::AfterReplicaC as i32;
        final_status.completed_replicas = 3;
        let mut transport = MockTransport {
            finalize_response: Some(status(pb::TapeFinalizationOutcome::Finalizing)),
            polls: VecDeque::from([
                Err(PollFinalizationError::Retryable),
                Ok(status(pb::TapeFinalizationOutcome::Finalizing)),
                Ok(final_status),
            ]),
            ..MockTransport::default()
        };
        let response = execute_with_transport(
            &mut transport,
            args().validate().unwrap(),
            true,
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(
            response.outcome,
            pb::TapeFinalizationOutcome::Finalized as i32
        );
        assert_eq!(transport.finalize_calls.len(), 1);
        assert_eq!(transport.get_calls.len(), 3);
        assert!(transport
            .get_calls
            .iter()
            .all(|uuid| uuid == Uuid::from_u128(1).as_bytes()));
    }

    #[test]
    fn json_status_has_stable_schema_and_typed_fields() {
        let mut final_status = status(pb::TapeFinalizationOutcome::FinalizedDegraded);
        final_status.progress = pb::TapeFinalizationProgress::AfterReplicaB as i32;
        final_status.completed_replicas = 2;
        final_status.edition_digest = vec![0xab; 32];
        final_status.replica_health.reverse();
        let mut out = Vec::new();
        print_finalization(&final_status, true, &mut out).unwrap();
        let value: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["schema"], FINALIZATION_JSON_SCHEMA);
        assert_eq!(value["kind"], "item");
        assert_eq!(value["data"]["outcome"], "finalized_degraded");
        assert_eq!(value["data"]["progress"], "after_replica_b");
        assert_eq!(value["data"]["completed_replicas"], 2);
        assert_eq!(value["data"]["replica_count"], 3);
        assert_eq!(value["data"]["terminal"], true);
        assert_eq!(value["data"]["operator_recovery_required"], false);
        assert_eq!(value["data"]["edition_digest"], "ab".repeat(32));
        assert_eq!(value["data"]["layout_digest"], Value::Null);
        assert_eq!(value["data"]["replica_health"][0]["replica_ordinal"], 1);
        assert_eq!(value["data"]["replica_health"][1]["replica_ordinal"], 2);
        assert_eq!(value["data"]["replica_health"][2]["replica_ordinal"], 3);
    }

    #[test]
    fn human_status_has_stable_operator_fields() {
        let mut final_status = status(pb::TapeFinalizationOutcome::RecoveryRequired);
        final_status.progress = pb::TapeFinalizationProgress::AfterSeparationAb as i32;
        final_status.completed_replicas = 1;
        final_status.replica_health[0].detail = "capsule B torn".to_string();
        let mut out = Vec::new();
        print_finalization(&final_status, false, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            concat!(
                "tape_uuid: 00000000-0000-0000-0000-000000000001\n",
                "operation_id: 00000000-0000-0000-0000-000000000003\n",
                "outcome: recovery_required\n",
                "trigger: operator_close_out\n",
                "progress: after_separation_ab\n",
                "completed_replicas: 1/3\n",
                "operator_recovery_required: true\n",
                "replica_health:\n",
                "  replica 1: pending (capsule B torn)\n",
                "  replica 2: pending\n",
                "  replica 3: pending\n",
                "detail: accepted\n",
            )
        );
    }

    #[test]
    fn polling_rejects_operation_identity_change() {
        let mut changed = status(pb::TapeFinalizationOutcome::Finalized);
        changed.operation_id = Uuid::from_u128(4).as_bytes().to_vec();
        let mut transport = MockTransport {
            finalize_response: Some(status(pb::TapeFinalizationOutcome::Finalizing)),
            polls: VecDeque::from([Ok(changed)]),
            ..MockTransport::default()
        };
        let error = execute_with_transport(
            &mut transport,
            args().validate().unwrap(),
            true,
            Duration::ZERO,
        )
        .unwrap_err();
        assert!(error
            .message
            .contains("changed the finalization operation_id"));
        assert_eq!(transport.finalize_calls.len(), 1);
    }

    #[test]
    fn status_validation_rejects_malformed_structured_fields() {
        let expected_tape = *Uuid::from_u128(1).as_bytes();

        let mut invalid = status(pb::TapeFinalizationOutcome::Finalized);
        invalid.operation_id = Uuid::nil().as_bytes().to_vec();
        assert!(validate_status(&invalid, &expected_tape, None)
            .unwrap_err()
            .message
            .contains("nil finalization operation_id"));

        let mut invalid = status(pb::TapeFinalizationOutcome::Finalized);
        invalid.layout_digest = vec![0; 31];
        assert!(validate_status(&invalid, &expected_tape, None)
            .unwrap_err()
            .message
            .contains("layout_digest with 31 bytes"));

        let mut invalid = status(pb::TapeFinalizationOutcome::Finalized);
        invalid
            .replica_health
            .push(invalid.replica_health[0].clone());
        assert!(validate_status(&invalid, &expected_tape, None)
            .unwrap_err()
            .message
            .contains("duplicate replica ordinal"));

        let mut invalid = status(pb::TapeFinalizationOutcome::Finalized);
        invalid.replica_health.pop();
        assert!(validate_status(&invalid, &expected_tape, None)
            .unwrap_err()
            .message
            .contains("2 replica-health rows"));
    }
}
