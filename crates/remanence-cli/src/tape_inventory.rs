//! Daemon-backed terminal-index inventory commands.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::process::ExitCode;

use clap::Args;
use remanence_api::pb;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{
    bytes_to_hex, bytes_to_uuid_text, connect_daemon, daemon_runtime, finish_daemon_client_result,
    print_json_envelope, DaemonClientError, DEFAULT_DAEMON_ENDPOINT,
};

const INVENTORY_JSON_SCHEMA: &str = "rem.tape.inventory.v1";
const INVENTORY_STREAM_JSON_SCHEMA: &str = "rem.tape.inventory.stream.v1";
const VERIFY_INDEX_JSON_SCHEMA: &str = "rem.tape.verify-index.v1";
const FAST_INVENTORY_BASIS: &str = "terminal_index_fast";

#[derive(Args, Clone, Debug)]
pub(crate) struct TapeInventoryArgs {
    /// Exact Remanence tape UUID. Voltags are not accepted.
    #[arg(long, value_name = "UUID")]
    pub(crate) tape_uuid: String,

    /// Emit stable CLI-shaped JSON.
    #[arg(long)]
    pub(crate) json: bool,

    /// Daemon gRPC endpoint URI.
    #[arg(long, value_name = "URI", default_value = DEFAULT_DAEMON_ENDPOINT)]
    pub(crate) endpoint: String,
}

impl TapeInventoryArgs {
    fn exact_tape_uuid(&self) -> Result<[u8; 16], String> {
        Uuid::parse_str(&self.tape_uuid)
            .map(|uuid| *uuid.as_bytes())
            .map_err(|error| format!("invalid tape_uuid {:?}: {error}", self.tape_uuid))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InventoryOutcome {
    Complete,
    Degraded,
    BotStructuralRecovered,
    BotStructuralRecoveryRequired,
}

impl InventoryOutcome {
    fn name(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Degraded => "degraded",
            Self::BotStructuralRecovered => "bot_structural_recovered",
            Self::BotStructuralRecoveryRequired => "bot_structural_recovery_required",
        }
    }

    fn requires_operator_recovery(self) -> bool {
        matches!(
            self,
            Self::BotStructuralRecovered | Self::BotStructuralRecoveryRequired
        )
    }
}

pub(crate) fn run_inventory(
    args: &TapeInventoryArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let tape_uuid = match args.exact_tape_uuid() {
        Ok(tape_uuid) => tape_uuid,
        Err(error) => {
            return finish_daemon_client_result(
                Err(DaemonClientError::client(error)),
                args.json,
                err,
            )
        }
    };

    let result = (|| -> Result<InventoryOutcome, DaemonClientError> {
        let runtime = daemon_runtime()?;
        let channel = runtime
            .block_on(connect_daemon(&args.endpoint))
            .map_err(DaemonClientError::client)?;
        let mut client = pb::catalog_client::CatalogClient::new(channel);
        let mut stream = runtime
            .block_on(client.get_tape_inventory(pb::TapeInventoryRequest {
                tape_uuid: tape_uuid.to_vec(),
            }))
            .map(tonic::Response::into_inner)
            .map_err(DaemonClientError::status)?;
        consume_inventory_stream(&runtime, &mut stream, tape_uuid, args.json, out)
    })();

    match result {
        Ok(outcome) if outcome.requires_operator_recovery() => ExitCode::from(1),
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => finish_daemon_client_result(Err(error), args.json, err),
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct StreamedAttemptCounts {
    replica_ordinal: u32,
    structural_entries: u64,
    object_rows: u64,
    rejected: bool,
}

fn consume_inventory_stream(
    runtime: &tokio::runtime::Runtime,
    stream: &mut tonic::Streaming<pb::TapeInventoryStreamItem>,
    tape_uuid: [u8; 16],
    json_output: bool,
    out: &mut dyn Write,
) -> Result<InventoryOutcome, DaemonClientError> {
    let mut attempts = BTreeMap::<u64, StreamedAttemptCounts>::new();
    let mut bot_rows = 0u64;
    let mut summary = None;
    while let Some(message) = runtime
        .block_on(stream.message())
        .map_err(DaemonClientError::status)?
    {
        let item = message
            .item
            .ok_or_else(|| DaemonClientError::client("daemon emitted an empty inventory item"))?;
        if summary.is_some() {
            return Err(DaemonClientError::client(
                "daemon emitted inventory data after its terminal summary",
            ));
        }
        use pb::tape_inventory_stream_item::Item;
        match item {
            Item::ReplicaAttemptStarted(start) => {
                if start.attempt_id == 0 || !(1..=3).contains(&start.replica_ordinal) {
                    return Err(DaemonClientError::client(
                        "daemon emitted an invalid inventory attempt",
                    ));
                }
                if attempts
                    .insert(
                        start.attempt_id,
                        StreamedAttemptCounts {
                            replica_ordinal: start.replica_ordinal,
                            ..StreamedAttemptCounts::default()
                        },
                    )
                    .is_some()
                {
                    return Err(DaemonClientError::client(
                        "daemon reused an inventory attempt id",
                    ));
                }
                print_inventory_attempt_started(&start, json_output, out)
                    .map_err(DaemonClientError::client)?;
            }
            Item::StructuralEntry(entry) => {
                let counts =
                    active_attempt_counts(&mut attempts, entry.attempt_id, entry.replica_ordinal)
                        .map_err(DaemonClientError::client)?;
                pb::TapeInventoryStructuralKind::try_from(entry.kind)
                    .ok()
                    .filter(|kind| *kind != pb::TapeInventoryStructuralKind::Unspecified)
                    .ok_or_else(|| {
                        DaemonClientError::client("daemon emitted an unknown structural kind")
                    })?;
                counts.structural_entries =
                    counts.structural_entries.checked_add(1).ok_or_else(|| {
                        DaemonClientError::client("streamed structural-entry count overflows u64")
                    })?;
                print_inventory_structural_entry(&entry, json_output, out)
                    .map_err(DaemonClientError::client)?;
            }
            Item::ObjectRow(row) => {
                let counts =
                    active_attempt_counts(&mut attempts, row.attempt_id, row.replica_ordinal)
                        .map_err(DaemonClientError::client)?;
                if row.object_id.is_empty() || row.representation.is_none() {
                    return Err(DaemonClientError::client(
                        "daemon emitted an incomplete Object recovery row",
                    ));
                }
                counts.object_rows = counts.object_rows.checked_add(1).ok_or_else(|| {
                    DaemonClientError::client("streamed Object-row count overflows u64")
                })?;
                print_inventory_object_row(&row, json_output, out)
                    .map_err(DaemonClientError::client)?;
            }
            Item::ReplicaAttemptRejected(rejected) => {
                let counts = active_attempt_counts(
                    &mut attempts,
                    rejected.attempt_id,
                    rejected.replica_ordinal,
                )
                .map_err(DaemonClientError::client)?;
                counts.rejected = true;
                print_inventory_attempt_rejected(&rejected, json_output, out)
                    .map_err(DaemonClientError::client)?;
            }
            Item::BotObject(object) => {
                pb::TapeInventoryBotObjectState::try_from(object.state)
                    .ok()
                    .filter(|state| *state != pb::TapeInventoryBotObjectState::Unspecified)
                    .ok_or_else(|| {
                        DaemonClientError::client("daemon emitted an unknown BOT Object state")
                    })?;
                bot_rows = bot_rows.checked_add(1).ok_or_else(|| {
                    DaemonClientError::client("streamed BOT Object count overflows u64")
                })?;
                print_inventory_bot_object(&object, json_output, out)
                    .map_err(DaemonClientError::client)?;
            }
            Item::Summary(inventory) => {
                summary = Some(inventory);
            }
        }
    }
    let inventory = summary
        .ok_or_else(|| DaemonClientError::client("daemon ended inventory without a summary"))?;
    let outcome = validate_inventory(&inventory, tape_uuid).map_err(DaemonClientError::client)?;
    match outcome {
        InventoryOutcome::Complete | InventoryOutcome::Degraded => {
            let selected = attempts
                .get(&inventory.selected_attempt_id)
                .ok_or_else(|| DaemonClientError::client("summary selected an unknown attempt"))?;
            if selected.rejected
                || selected.replica_ordinal != inventory.selected_replica_ordinal
                || selected.structural_entries != inventory.structural_entry_count
                || selected.object_rows != inventory.object_row_count
            {
                return Err(DaemonClientError::client(
                    "summary disagrees with the selected streamed row set",
                ));
            }
        }
        InventoryOutcome::BotStructuralRecovered => {
            let expected = inventory
                .recovered_object_count
                .checked_add(inventory.unknown_object_count)
                .and_then(|count| count.checked_add(inventory.incomplete_object_count))
                .ok_or_else(|| DaemonClientError::client("BOT summary count overflows u64"))?;
            if bot_rows != expected {
                return Err(DaemonClientError::client(
                    "BOT summary disagrees with streamed Object classifications",
                ));
            }
        }
        InventoryOutcome::BotStructuralRecoveryRequired => {}
    }
    if json_output {
        print_inventory_stream_json("summary", inventory_json(&inventory, outcome)?, out)
            .map_err(DaemonClientError::client)?;
    } else {
        print_inventory(&inventory, outcome, false, out).map_err(DaemonClientError::client)?;
    }
    Ok(outcome)
}

fn active_attempt_counts(
    attempts: &mut BTreeMap<u64, StreamedAttemptCounts>,
    attempt_id: u64,
    replica_ordinal: u32,
) -> Result<&mut StreamedAttemptCounts, String> {
    let counts = attempts
        .get_mut(&attempt_id)
        .ok_or_else(|| "daemon emitted a row before its inventory attempt".to_string())?;
    if counts.replica_ordinal != replica_ordinal || counts.rejected {
        return Err("daemon emitted a row for the wrong or rejected inventory attempt".to_string());
    }
    Ok(counts)
}

fn print_inventory_stream_json(
    event: &'static str,
    value: Value,
    out: &mut dyn Write,
) -> Result<(), String> {
    writeln!(
        out,
        "{}",
        json!({
            "schema": INVENTORY_STREAM_JSON_SCHEMA,
            "event": event,
            "value": value,
        })
    )
    .map_err(|error| error.to_string())
}

fn print_inventory_attempt_started(
    start: &pb::TapeInventoryReplicaAttemptStarted,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    if json_output {
        return print_inventory_stream_json(
            "replica_attempt_started",
            json!({
                "attempt_id": start.attempt_id.to_string(),
                "replica": replica_letter(start.replica_ordinal),
                "replica_ordinal": start.replica_ordinal,
            }),
            out,
        );
    }
    writeln!(
        out,
        "inventory_attempt: {} replica {} started (rows provisional until terminal summary)",
        start.attempt_id,
        replica_letter(start.replica_ordinal)
    )
    .map_err(|error| error.to_string())
}

fn structural_kind_name(kind: i32) -> Result<&'static str, String> {
    match pb::TapeInventoryStructuralKind::try_from(kind) {
        Ok(pb::TapeInventoryStructuralKind::Object) => Ok("object"),
        Ok(pb::TapeInventoryStructuralKind::ParitySidecar) => Ok("parity_sidecar"),
        Ok(pb::TapeInventoryStructuralKind::Bootstrap) => Ok("bootstrap"),
        Ok(pb::TapeInventoryStructuralKind::ParityMap) => Ok("parity_map"),
        Ok(pb::TapeInventoryStructuralKind::TapeIndexReplica) => Ok("tape_index_replica"),
        Ok(pb::TapeInventoryStructuralKind::IndexSeparationExtent) => Ok("index_separation_extent"),
        Ok(pb::TapeInventoryStructuralKind::Unspecified) | Err(_) => {
            Err("unknown inventory structural kind".to_string())
        }
    }
}

fn print_inventory_structural_entry(
    entry: &pb::TapeInventoryStructuralEntry,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    let kind = structural_kind_name(entry.kind)?;
    if json_output {
        return print_inventory_stream_json(
            "structural_entry",
            json!({
                "attempt_id": entry.attempt_id.to_string(),
                "replica": replica_letter(entry.replica_ordinal),
                "replica_ordinal": entry.replica_ordinal,
                "tape_file_number": entry.tape_file_number.to_string(),
                "kind": kind,
                "block_count": entry.block_count.to_string(),
                "first_parity_data_ordinal": entry.first_parity_data_ordinal.map(|value| value.to_string()),
                "protected_ordinal_start": entry.protected_ordinal_start.map(|value| value.to_string()),
                "protected_ordinal_end_exclusive": entry.protected_ordinal_end_exclusive.map(|value| value.to_string()),
                "epoch_id": entry.epoch_id.map(|value| value.to_string()),
            }),
            out,
        );
    }
    writeln!(
        out,
        "provisional_structural_entry: attempt={} replica={} tape_file={} kind={} blocks={}",
        entry.attempt_id,
        replica_letter(entry.replica_ordinal),
        entry.tape_file_number,
        kind,
        entry.block_count
    )
    .map_err(|error| error.to_string())
}

fn print_inventory_object_row(
    row: &pb::TapeInventoryObjectRow,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    use pb::tape_inventory_object_row::Representation;
    let representation = match row.representation.as_ref() {
        Some(Representation::Plaintext(plaintext)) => json!({
            "kind": "plaintext",
            "manifest_first_chunk_lba": plaintext.manifest_first_chunk_lba.to_string(),
            "manifest_size_bytes": plaintext.manifest_size_bytes.to_string(),
            "manifest_chunk_count": plaintext.manifest_chunk_count.to_string(),
            "manifest_sha256": bytes_to_hex(&plaintext.manifest_sha256),
        }),
        Some(Representation::Encrypted(encrypted)) => json!({
            "kind": "encrypted",
            "recipient_epoch_ids": encrypted.recipient_epoch_ids.iter().map(|id| bytes_to_hex(id)).collect::<Vec<_>>(),
            "metadata_frame_len": encrypted.metadata_frame_len.to_string(),
            "key_frame_len": encrypted.key_frame_len,
        }),
        None => return Err("Object recovery row omitted its representation".to_string()),
    };
    if json_output {
        return print_inventory_stream_json(
            "object_row",
            json!({
                "attempt_id": row.attempt_id.to_string(),
                "replica": replica_letter(row.replica_ordinal),
                "replica_ordinal": row.replica_ordinal,
                "tape_file_number": row.tape_file_number.to_string(),
                "stored_block_count": row.stored_block_count.to_string(),
                "object_id_hex": bytes_to_hex(&row.object_id),
                "representation": representation,
            }),
            out,
        );
    }
    writeln!(
        out,
        "provisional_object_row: attempt={} replica={} tape_file={} blocks={} object_id_hex={} representation={}",
        row.attempt_id,
        replica_letter(row.replica_ordinal),
        row.tape_file_number,
        row.stored_block_count,
        bytes_to_hex(&row.object_id),
        representation["kind"].as_str().unwrap_or("unknown")
    )
    .map_err(|error| error.to_string())
}

fn print_inventory_attempt_rejected(
    rejected: &pb::TapeInventoryReplicaAttemptRejected,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    if json_output {
        return print_inventory_stream_json(
            "replica_attempt_rejected",
            json!({
                "attempt_id": rejected.attempt_id.to_string(),
                "replica": replica_letter(rejected.replica_ordinal),
                "replica_ordinal": rejected.replica_ordinal,
                "failure_kind": rejected.failure_kind,
                "detail": rejected.detail,
            }),
            out,
        );
    }
    writeln!(
        out,
        "inventory_attempt: {} replica {} rejected [{}] {}",
        rejected.attempt_id,
        replica_letter(rejected.replica_ordinal),
        rejected.failure_kind,
        rejected.detail
    )
    .map_err(|error| error.to_string())
}

fn bot_object_state_name(state: i32) -> Result<&'static str, String> {
    match pb::TapeInventoryBotObjectState::try_from(state) {
        Ok(pb::TapeInventoryBotObjectState::Recovered) => Ok("recovered"),
        Ok(pb::TapeInventoryBotObjectState::Unknown) => Ok("unknown"),
        Ok(pb::TapeInventoryBotObjectState::Incomplete) => Ok("incomplete"),
        Ok(pb::TapeInventoryBotObjectState::Unspecified) | Err(_) => {
            Err("unknown BOT Object state".to_string())
        }
    }
}

fn print_inventory_bot_object(
    object: &pb::TapeInventoryBotObject,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    let state = bot_object_state_name(object.state)?;
    if json_output {
        return print_inventory_stream_json(
            "bot_object",
            json!({
                "tape_file_number": object.tape_file_number.to_string(),
                "stored_block_count": object.stored_block_count.to_string(),
                "object_id_hex": object.object_id.as_ref().map(|id| bytes_to_hex(id)),
                "state": state,
            }),
            out,
        );
    }
    writeln!(
        out,
        "bot_object: tape_file={} blocks={} object_id_hex={} state={}",
        object.tape_file_number,
        object.stored_block_count,
        object
            .object_id
            .as_ref()
            .map(|id| bytes_to_hex(id))
            .unwrap_or_else(|| "-".to_string()),
        state
    )
    .map_err(|error| error.to_string())
}

pub(crate) fn run_verify_index(
    args: &TapeInventoryArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let tape_uuid = match args.exact_tape_uuid() {
        Ok(tape_uuid) => tape_uuid,
        Err(error) => {
            return finish_daemon_client_result(
                Err(DaemonClientError::client(error)),
                args.json,
                err,
            )
        }
    };

    let result = (|| -> Result<bool, DaemonClientError> {
        let runtime = daemon_runtime()?;
        let channel = runtime
            .block_on(connect_daemon(&args.endpoint))
            .map_err(DaemonClientError::client)?;
        let mut client = pb::catalog_client::CatalogClient::new(channel);
        let verification = runtime
            .block_on(client.verify_tape_index(pb::VerifyTapeIndexRequest {
                tape_uuid: tape_uuid.to_vec(),
            }))
            .map(tonic::Response::into_inner)
            .map_err(DaemonClientError::status)?;
        let fast_inventory =
            validate_verification(&verification, tape_uuid).map_err(DaemonClientError::client)?;
        print_verification(&verification, fast_inventory, args.json, out)
            .map_err(DaemonClientError::client)?;
        Ok(pb::TapeIndexVerificationState::try_from(verification.state)
            == Ok(pb::TapeIndexVerificationState::VerifiedComplete))
    })();

    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => finish_daemon_client_result(Err(error), args.json, err),
    }
}

fn validate_verification(
    verification: &pb::TapeIndexVerification,
    expected_tape_uuid: [u8; 16],
) -> Result<Option<&pb::TapeInventory>, String> {
    require_exact_tape_uuid(&verification.tape_uuid, expected_tape_uuid)?;
    match pb::TapeIndexVerificationState::try_from(verification.state) {
        Ok(pb::TapeIndexVerificationState::DeferredFullPhysicalVerify) => {
            let inventory = verification.fast_inventory.as_ref().ok_or_else(|| {
                "daemon omitted the deferred fast terminal-index inventory".to_string()
            })?;
            validate_inventory(inventory, expected_tape_uuid)?;
            Ok(Some(inventory))
        }
        Ok(pb::TapeIndexVerificationState::VerifiedComplete) => {
            validate_full_verification(verification, true)?;
            Ok(None)
        }
        Ok(pb::TapeIndexVerificationState::VerifiedDegraded) => {
            validate_full_verification(verification, false)?;
            Ok(None)
        }
        Ok(pb::TapeIndexVerificationState::RecoveryRequired) => {
            validate_recovery_required_verification(verification, expected_tape_uuid)?;
            Ok(None)
        }
        Ok(pb::TapeIndexVerificationState::Unspecified) => {
            Err("daemon returned unspecified tape index verification state".to_string())
        }
        Err(_) => Err(format!(
            "daemon returned unknown tape index verification state {}",
            verification.state
        )),
    }
}

fn validate_full_verification(
    verification: &pb::TapeIndexVerification,
    require_complete: bool,
) -> Result<(), String> {
    if verification.fast_inventory.is_some() {
        return Err("verified response incorrectly carried fast-inventory evidence".to_string());
    }
    if verification.recovery_inventory.is_some() {
        return Err("verified response incorrectly carried BOT recovery evidence".to_string());
    }
    if verification.verification_basis != "measured_full_physical" {
        return Err(format!(
            "daemon returned unsupported verification basis {:?}",
            verification.verification_basis
        ));
    }
    validate_replica_health(&verification.replica_health)?;
    let all_replicas_valid = verification.replica_health.iter().all(|row| {
        pb::tape_index_replica_health::State::try_from(row.state)
            == Ok(pb::tape_index_replica_health::State::TapeIndexReplicaStateComplete)
    });
    if verification.separation_health.len() != 2
        || verification
            .separation_health
            .iter()
            .map(|gap| gap.separation_ordinal)
            .collect::<BTreeSet<_>>()
            != BTreeSet::from([1, 2])
    {
        return Err("verified response did not carry exact AB/BC separation evidence".to_string());
    }
    let mut all_separations_valid = true;
    for gap in &verification.separation_health {
        match pb::tape_index_separation_health::State::try_from(gap.state) {
            Ok(pb::tape_index_separation_health::State::TapeIndexSeparationStateValid) => {}
            Ok(pb::tape_index_separation_health::State::TapeIndexSeparationStateInvalid) => {
                all_separations_valid = false;
            }
            Ok(pb::tape_index_separation_health::State::TapeIndexSeparationStateUnknown)
            | Ok(pb::tape_index_separation_health::State::TapeIndexSeparationStateUnspecified)
            | Err(_) => {
                return Err("verified response carried unresolved separation evidence".to_string())
            }
        }
    }
    if require_complete && (!all_replicas_valid || !all_separations_valid) {
        return Err("verified-complete response carried degraded component evidence".to_string());
    }
    let expected_file_count = verification
        .verified_prefix_tape_file_count
        .checked_add(5)
        .ok_or_else(|| "verified physical tape-file count overflows u64".to_string())?;
    if require_complete && verification.measured_tape_file_count != expected_file_count {
        return Err(format!(
            "measured tape-file count {}, expected prefix {} plus five terminal files",
            verification.measured_tape_file_count, verification.verified_prefix_tape_file_count
        ));
    }
    if !require_complete
        && (verification.measured_tape_file_count < verification.verified_prefix_tape_file_count
            || verification.measured_tape_file_count > expected_file_count)
    {
        return Err(format!(
            "degraded measured tape-file count {} lies outside verified prefix {} through planned total {}",
            verification.measured_tape_file_count,
            verification.verified_prefix_tape_file_count,
            expected_file_count
        ));
    }
    if !require_complete
        && all_replicas_valid
        && all_separations_valid
        && verification.measured_tape_file_count == expected_file_count
    {
        return Err("verified-degraded response carried no degraded physical evidence".to_string());
    }
    if verification.measured_eod_lba == 0
        || verification.verified_prefix_tape_file_count == 0
        || verification.verified_prefix_record_count < verification.verified_prefix_tape_file_count
    {
        return Err("verified response carried impossible physical counts".to_string());
    }
    require_digest("edition", &verification.edition_digest)?;
    require_digest("layout", &verification.layout_digest)?;
    require_digest("payload", &verification.payload_digest)?;
    require_digest("canonical map", &verification.canonical_map_digest)?;
    Ok(())
}

fn validate_recovery_required_verification(
    verification: &pb::TapeIndexVerification,
    expected_tape_uuid: [u8; 16],
) -> Result<(), String> {
    if verification.fast_inventory.is_some()
        || verification.verification_basis != "bot_structural_recovery"
        || verification.measured_eod_lba == 0
        || verification.verified_prefix_tape_file_count != 0
        || verification.verified_prefix_record_count != 0
        || !verification.edition_digest.is_empty()
        || !verification.layout_digest.is_empty()
        || !verification.payload_digest.is_empty()
        || !verification.canonical_map_digest.is_empty()
    {
        return Err("recovery-required verification carried verified-prefix authority".to_string());
    }
    validate_replica_health(&verification.replica_health)?;
    if verification.separation_health.len() != 2
        || verification
            .separation_health
            .iter()
            .map(|gap| gap.separation_ordinal)
            .collect::<BTreeSet<_>>()
            != BTreeSet::from([1, 2])
        || verification.separation_health.iter().any(|gap| {
            pb::tape_index_separation_health::State::try_from(gap.state)
                != Ok(pb::tape_index_separation_health::State::TapeIndexSeparationStateUnknown)
        })
    {
        return Err("recovery-required verification omitted unknown gap evidence".to_string());
    }
    let recovery = verification
        .recovery_inventory
        .as_ref()
        .ok_or_else(|| "recovery-required verification omitted BOT scan evidence".to_string())?;
    if validate_inventory(recovery, expected_tape_uuid)? != InventoryOutcome::BotStructuralRecovered
    {
        return Err(
            "recovery-required verification attached the wrong inventory outcome".to_string(),
        );
    }
    if verification.measured_tape_file_count != recovery.structural_entry_count {
        return Err(
            "recovery-required verification disagreed with its BOT structural count".to_string(),
        );
    }
    Ok(())
}

fn validate_inventory(
    inventory: &pb::TapeInventory,
    expected_tape_uuid: [u8; 16],
) -> Result<InventoryOutcome, String> {
    require_exact_tape_uuid(&inventory.tape_uuid, expected_tape_uuid)?;
    let outcome = match pb::TapeInventoryOutcome::try_from(inventory.outcome) {
        Ok(pb::TapeInventoryOutcome::Complete) => InventoryOutcome::Complete,
        Ok(pb::TapeInventoryOutcome::Degraded) => InventoryOutcome::Degraded,
        Ok(pb::TapeInventoryOutcome::BotStructuralRecovered) => {
            InventoryOutcome::BotStructuralRecovered
        }
        Ok(pb::TapeInventoryOutcome::BotStructuralRecoveryRequired) => {
            InventoryOutcome::BotStructuralRecoveryRequired
        }
        Ok(pb::TapeInventoryOutcome::Unspecified) => {
            return Err("daemon returned unspecified tape inventory outcome".to_string())
        }
        Err(_) => {
            return Err(format!(
                "daemon returned unknown tape inventory outcome {}",
                inventory.outcome
            ))
        }
    };

    let expected_basis = match outcome {
        InventoryOutcome::Complete | InventoryOutcome::Degraded => FAST_INVENTORY_BASIS,
        InventoryOutcome::BotStructuralRecovered => "bot_structural_recovery",
        InventoryOutcome::BotStructuralRecoveryRequired => FAST_INVENTORY_BASIS,
    };
    if inventory.inventory_basis != expected_basis {
        return Err(format!(
            "daemon returned unsupported inventory basis {:?} for {}",
            inventory.inventory_basis,
            outcome.name()
        ));
    }

    validate_replica_health(&inventory.replica_health)?;
    let invalid_count = inventory
        .replica_health
        .iter()
        .filter(|row| {
            pb::tape_index_replica_health::State::try_from(row.state)
                == Ok(pb::tape_index_replica_health::State::TapeIndexReplicaStateInvalid)
        })
        .count();
    match outcome {
        InventoryOutcome::Complete | InventoryOutcome::Degraded => {
            if inventory.recovered_object_count != 0
                || inventory.unknown_object_count != 0
                || inventory.incomplete_object_count != 0
                || inventory.damaged_region_count != 0
            {
                return Err("fast terminal inventory carried BOT recovery evidence".to_string());
            }
            if inventory.selected_attempt_id == 0 {
                return Err("daemon omitted the selected inventory stream attempt".to_string());
            }
            if !(1..=3).contains(&inventory.selected_replica_ordinal) {
                return Err(format!(
                    "daemon returned invalid selected replica ordinal {}",
                    inventory.selected_replica_ordinal
                ));
            }
            if inventory.structural_entry_count == 0 {
                return Err("daemon returned an empty successful structural inventory".to_string());
            }
            require_digest("edition", &inventory.edition_digest)?;
            require_digest("layout", &inventory.layout_digest)?;
            require_digest("payload", &inventory.payload_digest)?;
            require_digest("canonical map", &inventory.canonical_map_digest)?;
            let selected = inventory
                .replica_health
                .iter()
                .find(|row| row.replica_ordinal == inventory.selected_replica_ordinal)
                .expect("validated A/B/C health rows contain the selected ordinal");
            if pb::tape_index_replica_health::State::try_from(selected.state)
                != Ok(pb::tape_index_replica_health::State::TapeIndexReplicaStateComplete)
            {
                return Err(
                    "daemon selected a terminal replica without a valid payload".to_string()
                );
            }
            for row in &inventory.replica_health {
                let state = pb::tape_index_replica_health::State::try_from(row.state)
                    .expect("replica states were validated above");
                if matches!(
                    state,
                    pb::tape_index_replica_health::State::TapeIndexReplicaStatePending
                        | pb::tape_index_replica_health::State::TapeIndexReplicaStateUnknown
                ) {
                    return Err(
                        "successful inventory carried unresolved replica evidence".to_string()
                    );
                }
                if row.replica_ordinal != inventory.selected_replica_ordinal
                    && state == pb::tape_index_replica_health::State::TapeIndexReplicaStateComplete
                {
                    return Err(
                        "fast inventory marked more than its selected replica payload complete"
                            .to_string(),
                    );
                }
                if row.replica_ordinal > inventory.selected_replica_ordinal
                    && state != pb::tape_index_replica_health::State::TapeIndexReplicaStateInvalid
                {
                    return Err(
                        "daemon did not honor terminal replica selection order C, then B, then A"
                            .to_string(),
                    );
                }
            }
            if outcome == InventoryOutcome::Complete && invalid_count != 0 {
                return Err("complete inventory carried degraded replica evidence".to_string());
            }
            if outcome == InventoryOutcome::Degraded && invalid_count == 0 {
                return Err("degraded inventory omitted invalid replica evidence".to_string());
            }
        }
        InventoryOutcome::BotStructuralRecovered => {
            if inventory.selected_attempt_id != 0 {
                return Err("BOT recovery selected a terminal stream attempt".to_string());
            }
            validate_bot_recovery(inventory, invalid_count)?;
        }
        InventoryOutcome::BotStructuralRecoveryRequired => {
            if inventory.selected_replica_ordinal != 0 || inventory.selected_attempt_id != 0 {
                return Err("BOT recovery outcome selected a terminal replica".to_string());
            }
            if inventory.structural_entry_count != 0
                || inventory.object_row_count != 0
                || !inventory.edition_digest.is_empty()
                || !inventory.layout_digest.is_empty()
                || !inventory.payload_digest.is_empty()
                || !inventory.canonical_map_digest.is_empty()
                || inventory.recovered_object_count != 0
                || inventory.unknown_object_count != 0
                || inventory.incomplete_object_count != 0
                || inventory.damaged_region_count != 0
            {
                return Err(
                    "BOT recovery outcome carried successful terminal inventory data".to_string(),
                );
            }
            if invalid_count != 3 {
                return Err(
                    "BOT recovery outcome did not mark every terminal replica invalid".to_string(),
                );
            }
        }
    }
    Ok(outcome)
}

fn validate_bot_recovery(
    inventory: &pb::TapeInventory,
    invalid_replica_count: usize,
) -> Result<(), String> {
    if inventory.selected_replica_ordinal != 0 || invalid_replica_count != 3 {
        return Err("BOT recovery incorrectly selected a terminal replica".to_string());
    }
    if inventory.structural_entry_count == 0 {
        return Err("BOT recovery returned an empty structural scan".to_string());
    }
    if !inventory.edition_digest.is_empty()
        || !inventory.layout_digest.is_empty()
        || !inventory.payload_digest.is_empty()
    {
        return Err("BOT recovery carried unsupported terminal-edition digests".to_string());
    }
    require_digest("BOT canonical map", &inventory.canonical_map_digest)?;
    let expected_complete = inventory
        .recovered_object_count
        .checked_add(inventory.unknown_object_count)
        .ok_or_else(|| "BOT recovery complete Object count overflows u64".to_string())?;
    if inventory.object_row_count != expected_complete {
        return Err("BOT recovery Object counts are inconsistent".to_string());
    }
    let total_candidates = expected_complete
        .checked_add(inventory.incomplete_object_count)
        .ok_or_else(|| "BOT recovery emitted Object count overflows u64".to_string())?;
    let structural_plus_torn = inventory
        .structural_entry_count
        .checked_add(u64::from(inventory.incomplete_object_count != 0))
        .ok_or_else(|| "BOT structural-plus-torn count overflows u64".to_string())?;
    if total_candidates > structural_plus_torn {
        return Err("BOT recovery Object counts exceed measured structural evidence".to_string());
    }
    Ok(())
}

fn require_exact_tape_uuid(actual: &[u8], expected: [u8; 16]) -> Result<(), String> {
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "daemon returned tape UUID {}, expected {}",
            bytes_to_uuid_text(actual),
            Uuid::from_bytes(expected)
        ))
    }
}

fn require_digest(name: &str, digest: &[u8]) -> Result<(), String> {
    if digest.len() == 32 {
        Ok(())
    } else {
        Err(format!(
            "daemon returned {name} digest with length {}, expected 32",
            digest.len()
        ))
    }
}

fn validate_replica_health(health: &[pb::TapeIndexReplicaHealth]) -> Result<(), String> {
    if health.len() != 3 {
        return Err(format!(
            "daemon returned {} replica-health rows, expected 3",
            health.len()
        ));
    }
    let ordinals = health
        .iter()
        .map(|row| row.replica_ordinal)
        .collect::<BTreeSet<_>>();
    if ordinals != BTreeSet::from([1, 2, 3]) {
        return Err(format!(
            "daemon returned invalid replica-health ordinals {ordinals:?}"
        ));
    }
    for row in health {
        replica_state_name(row.state)?;
    }
    Ok(())
}

fn replica_state_name(state: i32) -> Result<&'static str, String> {
    match pb::tape_index_replica_health::State::try_from(state) {
        Ok(pb::tape_index_replica_health::State::TapeIndexReplicaStatePending) => Ok("pending"),
        Ok(pb::tape_index_replica_health::State::TapeIndexReplicaStateComplete) => Ok("complete"),
        Ok(pb::tape_index_replica_health::State::TapeIndexReplicaStateEnvelopeValid) => {
            Ok("envelope_valid")
        }
        Ok(pb::tape_index_replica_health::State::TapeIndexReplicaStateInvalid) => Ok("invalid"),
        Ok(pb::tape_index_replica_health::State::TapeIndexReplicaStateUnknown) => Ok("unknown"),
        Ok(pb::tape_index_replica_health::State::TapeIndexReplicaStateUnspecified) => {
            Err("daemon returned unspecified terminal replica state".to_string())
        }
        Err(_) => Err(format!(
            "daemon returned unknown terminal replica state {state}"
        )),
    }
}

fn replica_letter(ordinal: u32) -> &'static str {
    match ordinal {
        1 => "A",
        2 => "B",
        3 => "C",
        _ => "-",
    }
}

fn separation_state_name(state: i32) -> Result<&'static str, String> {
    match pb::tape_index_separation_health::State::try_from(state) {
        Ok(pb::tape_index_separation_health::State::TapeIndexSeparationStateValid) => Ok("valid"),
        Ok(pb::tape_index_separation_health::State::TapeIndexSeparationStateInvalid) => {
            Ok("invalid")
        }
        Ok(pb::tape_index_separation_health::State::TapeIndexSeparationStateUnknown) => {
            Ok("unknown")
        }
        Ok(pb::tape_index_separation_health::State::TapeIndexSeparationStateUnspecified) => {
            Err("separation state is unspecified".to_string())
        }
        Err(_) => Err(format!("unknown separation state {state}")),
    }
}

fn inventory_json(
    inventory: &pb::TapeInventory,
    outcome: InventoryOutcome,
) -> Result<Value, String> {
    let mut health = inventory.replica_health.iter().collect::<Vec<_>>();
    health.sort_by_key(|row| row.replica_ordinal);
    let health = health
        .into_iter()
        .map(|row| {
            Ok(json!({
                "replica": replica_letter(row.replica_ordinal),
                "replica_ordinal": row.replica_ordinal,
                "state": replica_state_name(row.state)?,
                "detail": row.detail,
            }))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let selected = (inventory.selected_replica_ordinal != 0).then(|| {
        json!({
            "replica": replica_letter(inventory.selected_replica_ordinal),
            "replica_ordinal": inventory.selected_replica_ordinal,
        })
    });
    let digest = |bytes: &[u8]| (!bytes.is_empty()).then(|| bytes_to_hex(bytes));
    Ok(json!({
        "tape_uuid": bytes_to_uuid_text(&inventory.tape_uuid),
        "outcome": outcome.name(),
        "inventory_basis": inventory.inventory_basis,
        "selected_replica": selected,
        "selected_attempt_id": (inventory.selected_attempt_id != 0).then(|| inventory.selected_attempt_id.to_string()),
        "structural_entry_count": inventory.structural_entry_count.to_string(),
        "object_row_count": inventory.object_row_count.to_string(),
        "edition_digest": digest(&inventory.edition_digest),
        "layout_digest": digest(&inventory.layout_digest),
        "payload_digest": digest(&inventory.payload_digest),
        "canonical_map_digest": digest(&inventory.canonical_map_digest),
        "recovered_object_count": inventory.recovered_object_count.to_string(),
        "unknown_object_count": inventory.unknown_object_count.to_string(),
        "incomplete_object_count": inventory.incomplete_object_count.to_string(),
        "damaged_region_count": inventory.damaged_region_count.to_string(),
        "replica_health": health,
        "operator_recovery_required": outcome.requires_operator_recovery(),
        "detail": inventory.detail,
    }))
}

fn print_inventory(
    inventory: &pb::TapeInventory,
    outcome: InventoryOutcome,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    if json_output {
        return print_json_envelope(
            INVENTORY_JSON_SCHEMA,
            "tape_inventory",
            inventory_json(inventory, outcome)?,
            out,
        );
    }

    writeln!(
        out,
        "tape_uuid: {}",
        bytes_to_uuid_text(&inventory.tape_uuid)
    )
    .map_err(|error| error.to_string())?;
    writeln!(out, "outcome: {}", outcome.name()).map_err(|error| error.to_string())?;
    writeln!(out, "inventory_basis: {}", inventory.inventory_basis)
        .map_err(|error| error.to_string())?;
    writeln!(
        out,
        "selected_replica: {}",
        replica_letter(inventory.selected_replica_ordinal)
    )
    .map_err(|error| error.to_string())?;
    writeln!(
        out,
        "selected_attempt_id: {}",
        inventory.selected_attempt_id
    )
    .map_err(|error| error.to_string())?;
    writeln!(
        out,
        "structural_entry_count: {}",
        inventory.structural_entry_count
    )
    .map_err(|error| error.to_string())?;
    writeln!(out, "object_row_count: {}", inventory.object_row_count)
        .map_err(|error| error.to_string())?;
    if outcome == InventoryOutcome::BotStructuralRecovered {
        writeln!(
            out,
            "recovered_object_count: {}",
            inventory.recovered_object_count
        )
        .map_err(|error| error.to_string())?;
        writeln!(
            out,
            "unknown_object_count: {}",
            inventory.unknown_object_count
        )
        .map_err(|error| error.to_string())?;
        writeln!(
            out,
            "incomplete_object_count: {}",
            inventory.incomplete_object_count
        )
        .map_err(|error| error.to_string())?;
        writeln!(
            out,
            "damaged_region_count: {}",
            inventory.damaged_region_count
        )
        .map_err(|error| error.to_string())?;
    }
    writeln!(
        out,
        "operator_recovery_required: {}",
        outcome.requires_operator_recovery()
    )
    .map_err(|error| error.to_string())?;
    writeln!(out, "replica_health:").map_err(|error| error.to_string())?;
    let mut health = inventory.replica_health.iter().collect::<Vec<_>>();
    health.sort_by_key(|row| row.replica_ordinal);
    for row in health {
        writeln!(
            out,
            "  {}: {}{}",
            replica_letter(row.replica_ordinal),
            replica_state_name(row.state)?,
            if row.detail.is_empty() {
                String::new()
            } else {
                format!(" ({})", row.detail)
            }
        )
        .map_err(|error| error.to_string())?;
    }
    if !inventory.edition_digest.is_empty() {
        writeln!(
            out,
            "edition_digest: {}",
            bytes_to_hex(&inventory.edition_digest)
        )
        .map_err(|error| error.to_string())?;
        writeln!(
            out,
            "layout_digest: {}",
            bytes_to_hex(&inventory.layout_digest)
        )
        .map_err(|error| error.to_string())?;
        writeln!(
            out,
            "payload_digest: {}",
            bytes_to_hex(&inventory.payload_digest)
        )
        .map_err(|error| error.to_string())?;
        writeln!(
            out,
            "canonical_map_digest: {}",
            bytes_to_hex(&inventory.canonical_map_digest)
        )
        .map_err(|error| error.to_string())?;
    }
    writeln!(out, "detail: {}", inventory.detail).map_err(|error| error.to_string())
}

fn print_verification(
    verification: &pb::TapeIndexVerification,
    fast_inventory: Option<&pb::TapeInventory>,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    let state = pb::TapeIndexVerificationState::try_from(verification.state)
        .map_err(|_| "verification state is unknown".to_string())?;
    if state == pb::TapeIndexVerificationState::DeferredFullPhysicalVerify {
        let inventory = fast_inventory
            .ok_or_else(|| "deferred verification omitted fast inventory".to_string())?;
        let outcome = validate_inventory(
            inventory,
            <[u8; 16]>::try_from(verification.tape_uuid.as_slice())
                .map_err(|_| "verification tape UUID is not exactly 16 bytes".to_string())?,
        )?;
        if json_output {
            return print_json_envelope(
                VERIFY_INDEX_JSON_SCHEMA,
                "tape_verify_index",
                json!({
                    "tape_uuid": bytes_to_uuid_text(&verification.tape_uuid),
                    "verification_state": "deferred_full_physical_verify",
                    "verified": false,
                    "verification_basis": null,
                    "fast_inventory": inventory_json(inventory, outcome)?,
                    "detail": verification.detail,
                }),
                out,
            );
        }
        writeln!(out, "verification_state: deferred_full_physical_verify")
            .map_err(|error| error.to_string())?;
        writeln!(out, "verified: false").map_err(|error| error.to_string())?;
        writeln!(out, "verification_detail: {}", verification.detail)
            .map_err(|error| error.to_string())?;
        return print_inventory(inventory, outcome, false, out);
    }

    let mut replicas = verification.replica_health.iter().collect::<Vec<_>>();
    replicas.sort_by_key(|row| row.replica_ordinal);
    let replica_json = replicas
        .iter()
        .map(|row| {
            Ok(json!({
                "replica": replica_letter(row.replica_ordinal),
                "replica_ordinal": row.replica_ordinal,
                "state": replica_state_name(row.state)?,
                "detail": row.detail,
            }))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut separations = verification.separation_health.iter().collect::<Vec<_>>();
    separations.sort_by_key(|gap| gap.separation_ordinal);
    let separation_json = separations
        .iter()
        .map(|gap| {
            Ok(json!({
                "separation": if gap.separation_ordinal == 1 { "AB" } else { "BC" },
                "separation_ordinal": gap.separation_ordinal,
                "state": separation_state_name(gap.state)?,
                "verified_interior_record_count": gap.verified_interior_record_count.to_string(),
                "detail": gap.detail,
            }))
        })
        .collect::<Result<Vec<_>, String>>()?;
    if state == pb::TapeIndexVerificationState::RecoveryRequired {
        let recovery = verification
            .recovery_inventory
            .as_ref()
            .ok_or_else(|| "recovery-required response omitted BOT inventory".to_string())?;
        let outcome = validate_inventory(
            recovery,
            <[u8; 16]>::try_from(verification.tape_uuid.as_slice())
                .map_err(|_| "verification tape UUID is not exactly 16 bytes".to_string())?,
        )?;
        if json_output {
            return print_json_envelope(
                VERIFY_INDEX_JSON_SCHEMA,
                "tape_verify_index",
                json!({
                    "tape_uuid": bytes_to_uuid_text(&verification.tape_uuid),
                    "verification_state": "recovery_required",
                    "verified": false,
                    "verification_basis": verification.verification_basis,
                    "measured_eod_lba": verification.measured_eod_lba.to_string(),
                    "measured_tape_file_count": verification.measured_tape_file_count.to_string(),
                    "replica_health": replica_json,
                    "separation_health": separation_json,
                    "recovery_inventory": inventory_json(recovery, outcome)?,
                    "detail": verification.detail,
                }),
                out,
            );
        }
        writeln!(out, "verification_state: recovery_required")
            .map_err(|error| error.to_string())?;
        writeln!(out, "verified: false").map_err(|error| error.to_string())?;
        writeln!(out, "measured_eod_lba: {}", verification.measured_eod_lba)
            .map_err(|error| error.to_string())?;
        writeln!(
            out,
            "measured_tape_file_count: {}",
            verification.measured_tape_file_count
        )
        .map_err(|error| error.to_string())?;
        writeln!(out, "verification_detail: {}", verification.detail)
            .map_err(|error| error.to_string())?;
        return print_inventory(recovery, outcome, false, out);
    }
    let complete = state == pb::TapeIndexVerificationState::VerifiedComplete;
    let state_name = if complete {
        "verified_complete"
    } else {
        "verified_degraded"
    };
    if json_output {
        return print_json_envelope(
            VERIFY_INDEX_JSON_SCHEMA,
            "tape_verify_index",
            json!({
                "tape_uuid": bytes_to_uuid_text(&verification.tape_uuid),
                "verification_state": state_name,
                "verified": true,
                "complete": complete,
                "verification_basis": verification.verification_basis,
                "measured_eod_lba": verification.measured_eod_lba.to_string(),
                "verified_prefix_tape_file_count": verification.verified_prefix_tape_file_count.to_string(),
                "verified_prefix_record_count": verification.verified_prefix_record_count.to_string(),
                "measured_tape_file_count": verification.measured_tape_file_count.to_string(),
                "edition_digest": bytes_to_hex(&verification.edition_digest),
                "layout_digest": bytes_to_hex(&verification.layout_digest),
                "payload_digest": bytes_to_hex(&verification.payload_digest),
                "canonical_map_digest": bytes_to_hex(&verification.canonical_map_digest),
                "replica_health": replica_json,
                "separation_health": separation_json,
                "detail": verification.detail,
            }),
            out,
        );
    }

    writeln!(out, "verification_state: {state_name}").map_err(|error| error.to_string())?;
    writeln!(out, "verified: true").map_err(|error| error.to_string())?;
    writeln!(out, "complete: {complete}").map_err(|error| error.to_string())?;
    writeln!(
        out,
        "verification_basis: {}",
        verification.verification_basis
    )
    .map_err(|error| error.to_string())?;
    writeln!(out, "measured_eod_lba: {}", verification.measured_eod_lba)
        .map_err(|error| error.to_string())?;
    writeln!(
        out,
        "verified_prefix_tape_file_count: {}",
        verification.verified_prefix_tape_file_count
    )
    .map_err(|error| error.to_string())?;
    writeln!(
        out,
        "verified_prefix_record_count: {}",
        verification.verified_prefix_record_count
    )
    .map_err(|error| error.to_string())?;
    writeln!(
        out,
        "measured_tape_file_count: {}",
        verification.measured_tape_file_count
    )
    .map_err(|error| error.to_string())?;
    for (name, digest) in [
        ("edition_digest", &verification.edition_digest),
        ("layout_digest", &verification.layout_digest),
        ("payload_digest", &verification.payload_digest),
        ("canonical_map_digest", &verification.canonical_map_digest),
    ] {
        writeln!(out, "{name}: {}", bytes_to_hex(digest)).map_err(|error| error.to_string())?;
    }
    writeln!(out, "replica_health:").map_err(|error| error.to_string())?;
    for row in replicas {
        writeln!(
            out,
            "  {}: {} ({})",
            replica_letter(row.replica_ordinal),
            replica_state_name(row.state)?,
            row.detail
        )
        .map_err(|error| error.to_string())?;
    }
    writeln!(out, "separation_health:").map_err(|error| error.to_string())?;
    for gap in separations {
        writeln!(
            out,
            "  {}: {} ({} interior records; {})",
            if gap.separation_ordinal == 1 {
                "AB"
            } else {
                "BC"
            },
            separation_state_name(gap.state)?,
            gap.verified_interior_record_count,
            gap.detail
        )
        .map_err(|error| error.to_string())?;
    }
    writeln!(out, "verification_detail: {}", verification.detail).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    fn args(tape_uuid: &str) -> TapeInventoryArgs {
        TapeInventoryArgs {
            tape_uuid: tape_uuid.to_string(),
            json: false,
            endpoint: DEFAULT_DAEMON_ENDPOINT.to_string(),
        }
    }

    fn health(state_a: i32, state_b: i32, state_c: i32) -> Vec<pb::TapeIndexReplicaHealth> {
        [state_a, state_b, state_c]
            .into_iter()
            .enumerate()
            .map(|(index, state)| pb::TapeIndexReplicaHealth {
                replica_ordinal: u32::try_from(index + 1).unwrap(),
                state,
                detail: String::new(),
            })
            .collect()
    }

    fn complete_inventory(outcome: pb::TapeInventoryOutcome) -> pb::TapeInventory {
        let complete = pb::tape_index_replica_health::State::TapeIndexReplicaStateComplete as i32;
        let envelope =
            pb::tape_index_replica_health::State::TapeIndexReplicaStateEnvelopeValid as i32;
        let invalid = pb::tape_index_replica_health::State::TapeIndexReplicaStateInvalid as i32;
        pb::TapeInventory {
            tape_uuid: Uuid::from_u128(1).as_bytes().to_vec(),
            outcome: outcome as i32,
            selected_replica_ordinal: 3,
            replica_health: match outcome {
                pb::TapeInventoryOutcome::Complete => health(envelope, envelope, complete),
                pb::TapeInventoryOutcome::Degraded => health(invalid, envelope, complete),
                _ => unreachable!(),
            },
            structural_entry_count: 9,
            object_row_count: 8,
            edition_digest: vec![1; 32],
            layout_digest: vec![2; 32],
            payload_digest: vec![3; 32],
            canonical_map_digest: vec![4; 32],
            inventory_basis: FAST_INVENTORY_BASIS.to_string(),
            detail: "selected newest valid replica C".to_string(),
            recovered_object_count: 0,
            unknown_object_count: 0,
            incomplete_object_count: 0,
            damaged_region_count: 0,
            selected_attempt_id: 1,
        }
    }

    fn recovery_inventory() -> pb::TapeInventory {
        let invalid = pb::tape_index_replica_health::State::TapeIndexReplicaStateInvalid as i32;
        pb::TapeInventory {
            tape_uuid: Uuid::from_u128(1).as_bytes().to_vec(),
            outcome: pb::TapeInventoryOutcome::BotStructuralRecoveryRequired as i32,
            selected_replica_ordinal: 0,
            replica_health: health(invalid, invalid, invalid),
            structural_entry_count: 0,
            object_row_count: 0,
            edition_digest: Vec::new(),
            layout_digest: Vec::new(),
            payload_digest: Vec::new(),
            canonical_map_digest: Vec::new(),
            inventory_basis: FAST_INVENTORY_BASIS.to_string(),
            detail: "terminal replicas invalid; structural recovery from BOT is required"
                .to_string(),
            recovered_object_count: 0,
            unknown_object_count: 0,
            incomplete_object_count: 0,
            damaged_region_count: 0,
            selected_attempt_id: 0,
        }
    }

    fn bot_recovered_inventory() -> pb::TapeInventory {
        let invalid = pb::tape_index_replica_health::State::TapeIndexReplicaStateInvalid as i32;
        pb::TapeInventory {
            tape_uuid: Uuid::from_u128(1).as_bytes().to_vec(),
            outcome: pb::TapeInventoryOutcome::BotStructuralRecovered as i32,
            selected_replica_ordinal: 0,
            replica_health: health(invalid, invalid, invalid),
            structural_entry_count: 4,
            object_row_count: 2,
            edition_digest: Vec::new(),
            layout_digest: Vec::new(),
            payload_digest: Vec::new(),
            canonical_map_digest: vec![4; 32],
            inventory_basis: "bot_structural_recovery".to_string(),
            detail: "BOT recovery classified Object candidates".to_string(),
            recovered_object_count: 1,
            unknown_object_count: 1,
            incomplete_object_count: 1,
            damaged_region_count: 1,
            selected_attempt_id: 0,
        }
    }

    #[test]
    fn exact_uuid_argument_rejects_voltags_and_accepts_uuid() {
        assert!(args("TAPE001L9").exact_tape_uuid().is_err());
        assert_eq!(
            args(&Uuid::from_u128(1).to_string()).exact_tape_uuid(),
            Ok(*Uuid::from_u128(1).as_bytes())
        );
    }

    #[test]
    fn rem_tape_commands_route_exact_uuid_args_to_daemon_clients() {
        let tape_uuid = Uuid::from_u128(1).to_string();
        let inventory: crate::ParsedCli =
            crate::Cli::parse_from(["rem", "tape", "inventory", "--tape-uuid", &tape_uuid]).into();
        match inventory.command {
            crate::Command::TapeInventoryClient(args) => assert_eq!(args.tape_uuid, tape_uuid),
            other => panic!("unexpected inventory dispatch: {other:?}"),
        }

        let verify: crate::ParsedCli = crate::Cli::parse_from([
            "rem",
            "tape",
            "verify-index",
            "--tape-uuid",
            &tape_uuid,
            "--json",
        ])
        .into();
        match verify.command {
            crate::Command::TapeVerifyIndexClient(args) => assert!(args.json),
            other => panic!("unexpected verify-index dispatch: {other:?}"),
        }
    }

    #[test]
    fn inventory_json_is_stable_and_exposes_degraded_replica_evidence() {
        let inventory = complete_inventory(pb::TapeInventoryOutcome::Degraded);
        let outcome = validate_inventory(&inventory, *Uuid::from_u128(1).as_bytes()).unwrap();
        let mut out = Vec::new();
        print_inventory(&inventory, outcome, true, &mut out).unwrap();
        let envelope: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(envelope["schema"], INVENTORY_JSON_SCHEMA);
        assert_eq!(envelope["data"]["outcome"], "degraded");
        assert_eq!(envelope["data"]["selected_replica"]["replica"], "C");
        assert_eq!(envelope["data"]["replica_health"][0]["state"], "invalid");
        assert_eq!(envelope["data"]["structural_entry_count"], "9");
        assert_eq!(envelope["data"]["object_row_count"], "8");
        assert_eq!(envelope["data"]["operator_recovery_required"], false);
    }

    #[test]
    fn inventory_json_preserves_structural_u64_max_as_decimal_text() {
        let mut inventory = complete_inventory(pb::TapeInventoryOutcome::Complete);
        inventory.structural_entry_count = u64::MAX;
        inventory.object_row_count = u64::MAX;
        inventory.recovered_object_count = u64::MAX;
        inventory.unknown_object_count = u64::MAX;
        inventory.incomplete_object_count = u64::MAX;
        inventory.damaged_region_count = u64::MAX;

        let value = inventory_json(&inventory, InventoryOutcome::Complete).unwrap();
        for field in [
            "structural_entry_count",
            "object_row_count",
            "recovered_object_count",
            "unknown_object_count",
            "incomplete_object_count",
            "damaged_region_count",
        ] {
            assert_eq!(value[field], u64::MAX.to_string());
        }
    }

    #[test]
    fn bot_recovery_is_explicit_and_cannot_validate_as_empty_success() {
        let inventory = recovery_inventory();
        let outcome = validate_inventory(&inventory, *Uuid::from_u128(1).as_bytes()).unwrap();
        assert!(outcome.requires_operator_recovery());
        let mut out = Vec::new();
        print_inventory(&inventory, outcome, false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("outcome: bot_structural_recovery_required"));
        assert!(text.contains("operator_recovery_required: true"));

        let mut false_success = inventory;
        false_success.outcome = pb::TapeInventoryOutcome::Complete as i32;
        assert!(validate_inventory(&false_success, *Uuid::from_u128(1).as_bytes()).is_err());
    }

    #[test]
    fn completed_bot_recovery_exposes_typed_object_classifications() {
        let inventory = bot_recovered_inventory();
        let outcome = validate_inventory(&inventory, *Uuid::from_u128(1).as_bytes()).unwrap();
        assert_eq!(outcome, InventoryOutcome::BotStructuralRecovered);
        assert!(outcome.requires_operator_recovery());
        let mut out = Vec::new();
        print_inventory(&inventory, outcome, true, &mut out).unwrap();
        let envelope: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(envelope["data"]["outcome"], "bot_structural_recovered");
        assert_eq!(envelope["data"]["recovered_object_count"], "1");
        assert_eq!(envelope["data"]["unknown_object_count"], "1");
        assert_eq!(envelope["data"]["incomplete_object_count"], "1");
    }

    #[test]
    fn verify_index_json_truthfully_reports_deferred_not_verified() {
        let inventory = complete_inventory(pb::TapeInventoryOutcome::Complete);
        let verification = pb::TapeIndexVerification {
            tape_uuid: Uuid::from_u128(1).as_bytes().to_vec(),
            state: pb::TapeIndexVerificationState::DeferredFullPhysicalVerify as i32,
            fast_inventory: Some(inventory),
            detail: "full physical verification is not implemented".to_string(),
            ..Default::default()
        };
        let inventory =
            validate_verification(&verification, *Uuid::from_u128(1).as_bytes()).unwrap();
        let mut out = Vec::new();
        print_verification(&verification, inventory, true, &mut out).unwrap();
        let envelope: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(envelope["schema"], VERIFY_INDEX_JSON_SCHEMA);
        assert_eq!(
            envelope["data"]["verification_state"],
            "deferred_full_physical_verify"
        );
        assert_eq!(envelope["data"]["verified"], false);
    }

    #[test]
    fn verify_index_json_reports_full_measured_evidence() {
        let complete = pb::tape_index_replica_health::State::TapeIndexReplicaStateComplete as i32;
        let verification = pb::TapeIndexVerification {
            tape_uuid: Uuid::from_u128(1).as_bytes().to_vec(),
            state: pb::TapeIndexVerificationState::VerifiedComplete as i32,
            fast_inventory: None,
            detail: "all physical evidence validated".to_string(),
            replica_health: health(complete, complete, complete),
            separation_health: vec![
                pb::TapeIndexSeparationHealth {
                    separation_ordinal: 1,
                    state: pb::tape_index_separation_health::State::TapeIndexSeparationStateValid
                        as i32,
                    verified_interior_record_count: 4094,
                    detail: "valid".to_string(),
                },
                pb::TapeIndexSeparationHealth {
                    separation_ordinal: 2,
                    state: pb::tape_index_separation_health::State::TapeIndexSeparationStateValid
                        as i32,
                    verified_interior_record_count: 4094,
                    detail: "valid".to_string(),
                },
            ],
            measured_eod_lba: 12_300,
            verified_prefix_tape_file_count: 4,
            verified_prefix_record_count: 11,
            measured_tape_file_count: 9,
            edition_digest: vec![1; 32],
            layout_digest: vec![2; 32],
            payload_digest: vec![3; 32],
            canonical_map_digest: vec![4; 32],
            verification_basis: "measured_full_physical".to_string(),
            recovery_inventory: None,
        };
        let fast_inventory =
            validate_verification(&verification, *Uuid::from_u128(1).as_bytes()).unwrap();
        assert!(fast_inventory.is_none());
        let mut out = Vec::new();
        print_verification(&verification, fast_inventory, true, &mut out).unwrap();
        let envelope: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(envelope["data"]["verification_state"], "verified_complete");
        assert_eq!(envelope["data"]["verified"], true);
        assert_eq!(
            envelope["data"]["verification_basis"],
            "measured_full_physical"
        );
        assert_eq!(
            envelope["data"]["replica_health"].as_array().unwrap().len(),
            3
        );
        assert_eq!(
            envelope["data"]["separation_health"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(envelope["data"]["measured_eod_lba"], "12300");
        assert_eq!(envelope["data"]["verified_prefix_tape_file_count"], "4");
        assert_eq!(envelope["data"]["verified_prefix_record_count"], "11");
        assert_eq!(envelope["data"]["measured_tape_file_count"], "9");
        assert_eq!(
            envelope["data"]["separation_health"][0]["verified_interior_record_count"],
            "4094"
        );
    }

    #[test]
    fn verify_index_json_reports_degraded_component_evidence() {
        let complete = pb::tape_index_replica_health::State::TapeIndexReplicaStateComplete as i32;
        let invalid = pb::tape_index_replica_health::State::TapeIndexReplicaStateInvalid as i32;
        let valid_gap =
            pb::tape_index_separation_health::State::TapeIndexSeparationStateValid as i32;
        let verification = pb::TapeIndexVerification {
            tape_uuid: Uuid::from_u128(1).as_bytes().to_vec(),
            state: pb::TapeIndexVerificationState::VerifiedDegraded as i32,
            detail: "replica A payload invalid".to_string(),
            replica_health: health(invalid, complete, complete),
            separation_health: (1..=2)
                .map(|separation_ordinal| pb::TapeIndexSeparationHealth {
                    separation_ordinal,
                    state: valid_gap,
                    verified_interior_record_count: 4094,
                    detail: "valid".to_string(),
                })
                .collect(),
            measured_eod_lba: 12_300,
            verified_prefix_tape_file_count: 4,
            verified_prefix_record_count: 11,
            measured_tape_file_count: 9,
            edition_digest: vec![1; 32],
            layout_digest: vec![2; 32],
            payload_digest: vec![3; 32],
            canonical_map_digest: vec![4; 32],
            verification_basis: "measured_full_physical".to_string(),
            ..Default::default()
        };

        let inventory =
            validate_verification(&verification, *Uuid::from_u128(1).as_bytes()).unwrap();
        let mut out = Vec::new();
        print_verification(&verification, inventory, true, &mut out).unwrap();
        let envelope: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(envelope["data"]["verification_state"], "verified_degraded");
        assert_eq!(envelope["data"]["verified"], true);
        assert_eq!(envelope["data"]["complete"], false);
        assert_eq!(envelope["data"]["replica_health"][0]["state"], "invalid");
    }

    #[test]
    fn verify_index_json_reports_real_bot_recovery_evidence() {
        let invalid = pb::tape_index_replica_health::State::TapeIndexReplicaStateInvalid as i32;
        let unknown_gap =
            pb::tape_index_separation_health::State::TapeIndexSeparationStateUnknown as i32;
        let recovery = bot_recovered_inventory();
        let verification = pb::TapeIndexVerification {
            tape_uuid: Uuid::from_u128(1).as_bytes().to_vec(),
            state: pb::TapeIndexVerificationState::RecoveryRequired as i32,
            detail: "no canonical survivor".to_string(),
            replica_health: health(invalid, invalid, invalid),
            separation_health: (1..=2)
                .map(|separation_ordinal| pb::TapeIndexSeparationHealth {
                    separation_ordinal,
                    state: unknown_gap,
                    verified_interior_record_count: 0,
                    detail: "unknown".to_string(),
                })
                .collect(),
            measured_eod_lba: 12_300,
            measured_tape_file_count: recovery.structural_entry_count,
            verification_basis: "bot_structural_recovery".to_string(),
            recovery_inventory: Some(recovery),
            ..Default::default()
        };

        let inventory =
            validate_verification(&verification, *Uuid::from_u128(1).as_bytes()).unwrap();
        let mut out = Vec::new();
        print_verification(&verification, inventory, true, &mut out).unwrap();
        let envelope: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(envelope["data"]["verification_state"], "recovery_required");
        assert_eq!(envelope["data"]["verified"], false);
        assert_eq!(envelope["data"]["measured_eod_lba"], "12300");
        assert_eq!(
            envelope["data"]["recovery_inventory"]["outcome"],
            "bot_structural_recovered"
        );
    }
}
