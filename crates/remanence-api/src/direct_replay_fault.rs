//! Explicitly gated checkpoint crash cut for the live direct-replay scenario.
//!
//! This private hook is inert unless both exact environment variables are set.
//! It publishes durable evidence only after the direct writer's checkpoint
//! append has fsynced, then aborts before the corresponding SQLite projection.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use remanence_state::CheckpointJournalRecord;
use serde_json::{json, Value};
use uuid::Uuid;

pub(crate) const ENABLE_ENV: &str = "REM_WOR_DIRECT_REPLAY_FAULT_ENABLE";
pub(crate) const PLAN_ENV: &str = "REM_WOR_DIRECT_REPLAY_FAULT_PLAN";
const ENABLE_VALUE: &str = "direct-replay-after-checkpoint-v1-abort";
const PLAN_SCHEMA: &str = "rem.wor.direct-replay-fault-plan.v1";
const EVIDENCE_SCHEMA: &str = "rem.wor.direct-replay-fault-evidence.v1";
const FAULT_ROOT: &str = "/tmp/system-harness/scenario-wor/direct-replay-faults";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectReplayFaultPlan {
    tape_uuid: [u8; 16],
    caller_object_id: String,
    nonce: Uuid,
    evidence_path: PathBuf,
}

impl DirectReplayFaultPlan {
    pub(crate) fn from_env_for_object(
        tape_uuid: [u8; 16],
        caller_object_id: &str,
    ) -> Result<Option<Self>, String> {
        let enabled = std::env::var_os(ENABLE_ENV);
        let plan_path = std::env::var_os(PLAN_ENV);
        if enabled.is_none() && plan_path.is_none() {
            return Ok(None);
        }
        let enabled = enabled.ok_or_else(|| {
            format!("{PLAN_ENV} is set but the explicit {ENABLE_ENV} opt-in is absent")
        })?;
        if enabled != OsStr::new(ENABLE_VALUE) {
            return Err(format!(
                "{ENABLE_ENV} must equal the exact WOR-only value {ENABLE_VALUE:?}"
            ));
        }
        let plan_path = PathBuf::from(plan_path.ok_or_else(|| {
            format!("{ENABLE_ENV} is set but the exact {PLAN_ENV} path is absent")
        })?);
        require_existing_confined(&plan_path, "fault plan")?;
        let value: Value = serde_json::from_slice(&fs::read(&plan_path).map_err(|error| {
            format!(
                "read direct-replay fault plan {}: {error}",
                plan_path.display()
            )
        })?)
        .map_err(|error| {
            format!(
                "decode direct-replay fault plan {}: {error}",
                plan_path.display()
            )
        })?;
        let plan = Self::parse(&value)?;
        if plan.tape_uuid != tape_uuid || plan.caller_object_id != caller_object_id {
            return Err(
                "direct-replay fault plan does not match the exact tape and caller object id"
                    .to_string(),
            );
        }
        require_future_confined(&plan.evidence_path, "evidence")?;
        if plan.evidence_path.exists() {
            return Err(format!(
                "direct-replay fault evidence already exists at {}",
                plan.evidence_path.display()
            ));
        }
        Ok(Some(plan))
    }

    fn parse(value: &Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or_else(|| "direct-replay fault plan must be a JSON object".to_string())?;
        let required = [
            "schema",
            "tape_uuid",
            "caller_object_id",
            "nonce",
            "evidence_path",
        ];
        if object.len() != required.len() || required.iter().any(|key| !object.contains_key(*key)) {
            return Err(format!(
                "direct-replay fault plan must contain exactly {}",
                required.join(", ")
            ));
        }
        require_text(object, "schema", PLAN_SCHEMA)?;
        let tape_uuid_text = text_field(object, "tape_uuid")?;
        let tape_uuid = Uuid::parse_str(tape_uuid_text)
            .map_err(|error| format!("invalid direct-replay fault tape_uuid: {error}"))?;
        if tape_uuid.to_string() != tape_uuid_text {
            return Err(
                "direct-replay fault tape_uuid must be canonical lowercase UUID text".into(),
            );
        }
        let caller_object_id = text_field(object, "caller_object_id")?.to_string();
        let nonce_text = text_field(object, "nonce")?;
        let nonce = Uuid::parse_str(nonce_text)
            .map_err(|error| format!("invalid direct-replay fault nonce: {error}"))?;
        if nonce.is_nil() || nonce.to_string() != nonce_text {
            return Err("direct-replay fault nonce must be a non-nil canonical UUID".into());
        }
        let evidence_path = PathBuf::from(text_field(object, "evidence_path")?);
        require_lexically_confined(&evidence_path, "evidence")?;
        Ok(Self {
            tape_uuid: *tape_uuid.as_bytes(),
            caller_object_id,
            nonce,
            evidence_path,
        })
    }

    fn publish(&self, record: &CheckpointJournalRecord) -> Result<(), String> {
        if record.tape_uuid != self.tape_uuid || record.objects.len() != 1 {
            return Err(
                "direct-replay checkpoint record does not match the planned tape/object"
                    .to_string(),
            );
        }
        let projection = &record.objects[0];
        if projection.object.caller_object_id.as_deref() != Some(self.caller_object_id.as_str()) {
            return Err("direct-replay checkpoint caller object id changed before the cut".into());
        }
        let evidence = json!({
            "schema": EVIDENCE_SCHEMA,
            "tape_uuid": Uuid::from_bytes(self.tape_uuid).to_string(),
            "caller_object_id": self.caller_object_id,
            "object_id": projection.object.object_id,
            "nonce": self.nonce.to_string(),
            "checkpoint_ordinal": record.ordinal.to_string(),
            "checkpoint_eod_partition": record.eod_partition,
            "checkpoint_eod_lba": record.eod_lba.to_string(),
        });
        write_durable_new_json(&self.evidence_path, &evidence)
    }

    pub(crate) fn abort_after_checkpoint_append(
        &self,
        record: &CheckpointJournalRecord,
    ) -> Result<(), String> {
        self.publish(record)?;
        std::process::abort()
    }
}

fn text_field<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
) -> Result<&'a str, String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("direct-replay fault plan field {field:?} must be nonempty text"))
}

fn require_text(
    object: &serde_json::Map<String, Value>,
    field: &str,
    expected: &str,
) -> Result<(), String> {
    let actual = text_field(object, field)?;
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "direct-replay fault plan field {field:?} must equal {expected:?}, got {actual:?}"
        ))
    }
}

fn require_lexically_confined(path: &Path, label: &str) -> Result<(), String> {
    if !path.is_absolute()
        || !path.starts_with(FAULT_ROOT)
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!(
            "direct-replay fault {label} path {} must be absolute and confined beneath {FAULT_ROOT}",
            path.display()
        ));
    }
    Ok(())
}

fn require_existing_confined(path: &Path, label: &str) -> Result<(), String> {
    require_lexically_confined(path, label)?;
    let root = fs::canonicalize(FAULT_ROOT)
        .map_err(|error| format!("resolve direct-replay fault root {FAULT_ROOT}: {error}"))?;
    let resolved = fs::canonicalize(path).map_err(|error| {
        format!(
            "resolve direct-replay fault {label} {}: {error}",
            path.display()
        )
    })?;
    if !resolved.starts_with(&root) {
        return Err(format!(
            "resolved direct-replay fault {label} {} escapes {FAULT_ROOT}",
            path.display()
        ));
    }
    Ok(())
}

fn require_future_confined(path: &Path, label: &str) -> Result<(), String> {
    require_lexically_confined(path, label)?;
    let root = fs::canonicalize(FAULT_ROOT)
        .map_err(|error| format!("resolve direct-replay fault root {FAULT_ROOT}: {error}"))?;
    let mut ancestor = path.parent();
    while ancestor.is_some_and(|candidate| !candidate.exists()) {
        ancestor = ancestor.and_then(Path::parent);
    }
    let ancestor = ancestor.ok_or_else(|| {
        format!(
            "direct-replay fault {label} path {} has no existing ancestor",
            path.display()
        )
    })?;
    let resolved = fs::canonicalize(ancestor).map_err(|error| {
        format!(
            "resolve direct-replay fault {label} ancestor {}: {error}",
            ancestor.display()
        )
    })?;
    if !resolved.starts_with(root) {
        return Err(format!(
            "resolved direct-replay fault {label} path {} escapes {FAULT_ROOT}",
            path.display()
        ));
    }
    Ok(())
}

fn write_durable_new_json(path: &Path, value: &Value) -> Result<(), String> {
    require_future_confined(path, "evidence")?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("direct-replay evidence {} has no parent", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "create direct-replay evidence directory {}: {error}",
            parent.display()
        )
    })?;
    require_existing_confined(parent, "evidence directory")?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(|error| {
            format!(
                "create direct-replay temporary evidence {}: {error}",
                temporary.display()
            )
        })?;
    serde_json::to_writer_pretty(&mut file, value)
        .map_err(|error| format!("encode direct-replay evidence: {error}"))?;
    file.write_all(b"\n")
        .map_err(|error| format!("finish direct-replay evidence: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("fsync direct-replay evidence: {error}"))?;
    drop(file);
    fs::hard_link(&temporary, path)
        .map_err(|error| format!("publish direct-replay evidence {}: {error}", path.display()))?;
    fs::remove_file(&temporary).map_err(|error| {
        format!(
            "remove direct-replay temporary evidence {}: {error}",
            temporary.display()
        )
    })?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("fsync direct-replay evidence directory: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use remanence_state::{
        CheckpointObjectProjection, CheckpointObjectRecoveryRepresentation,
        CheckpointObjectRecoveryRow, NativeObjectCopyProjectionInput,
        NativeObjectFileProjectionInput, NativeObjectProjectionInput,
    };

    #[test]
    fn exact_plan_publishes_checkpoint_authority_evidence() {
        fs::create_dir_all(FAULT_ROOT).expect("create fault root");
        let temp = tempfile::Builder::new()
            .prefix("direct-replay-fault-test-")
            .tempdir_in(FAULT_ROOT)
            .expect("fault tempdir");
        let tape_uuid = [0x31; 16];
        let object_id = Uuid::from_bytes([0x32; 16]).to_string();
        let plan = DirectReplayFaultPlan {
            tape_uuid,
            caller_object_id: "direct-replay-test".to_string(),
            nonce: Uuid::new_v4(),
            evidence_path: temp.path().join("evidence.json"),
        };
        let record = CheckpointJournalRecord {
            ordinal: 7,
            committed_object_count: 1,
            eod_partition: 0,
            eod_lba: 19,
            tape_uuid,
            batch_id: [0x33; 16],
            next_tape_file_number: 2,
            block_size: 256 * 1024,
            objects: vec![CheckpointObjectProjection {
                object: NativeObjectProjectionInput {
                    object_id: object_id.clone(),
                    caller_object_id: Some(plan.caller_object_id.clone()),
                    body_format: "rem-object-v1".to_string(),
                    logical_size_bytes: Some(1),
                    content_hash: Some(vec![0x34; 32]),
                    metadata_hash: Some(vec![0x35; 32]),
                    created_at_utc: Some("2026-08-11T00:00:00Z".to_string()),
                },
                files: vec![NativeObjectFileProjectionInput {
                    object_id: object_id.clone(),
                    file_id: "file".to_string(),
                    path: "payload.bin".to_string(),
                    size_bytes: 1,
                    file_sha256: vec![0x34; 32],
                    first_chunk_lba: Some(1),
                    chunk_count: 1,
                    mtime: None,
                    executable: Some(false),
                }],
                copy: NativeObjectCopyProjectionInput {
                    object_id: object_id.clone(),
                    tape_uuid,
                    tape_file_number: 1,
                    first_body_lba: 1,
                    first_parity_data_ordinal: None,
                    protected_until_ordinal: None,
                    status: "committed".to_string(),
                    representation: "plaintext".to_string(),
                    recipient_epoch_ids: None,
                    metadata_frame_len: None,
                    plaintext_digest: Some(vec![0x36; 32]),
                    stored_digest: Some(vec![0x36; 32]),
                },
                block_size: 256 * 1024,
                block_count: 1,
                fresh_tape: true,
                total_committed_ordinals: 1,
                object_recovery_row: CheckpointObjectRecoveryRow {
                    tape_file_number: 1,
                    stored_block_count: 1,
                    object_id: object_id.clone().into_bytes(),
                    representation: CheckpointObjectRecoveryRepresentation::Plaintext {
                        manifest_first_chunk_lba: 1,
                        manifest_size_bytes: 1,
                        manifest_chunk_count: 1,
                        manifest_sha256: [0x37; 32],
                    },
                },
            }],
            scheme: None,
            object_tape_file_bundles: Vec::new(),
            barrier_bundle: None,
            terminal_finalization: None,
            sealed_after_write: false,
        };

        plan.publish(&record).expect("publish evidence");
        let evidence: Value =
            serde_json::from_slice(&fs::read(&plan.evidence_path).expect("read evidence"))
                .expect("decode evidence");
        assert_eq!(evidence["schema"], EVIDENCE_SCHEMA);
        assert_eq!(evidence["object_id"], object_id);
        assert_eq!(evidence["checkpoint_ordinal"], "7");
        assert_eq!(evidence["checkpoint_eod_lba"], "19");
    }
}
