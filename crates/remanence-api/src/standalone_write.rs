//! Standalone version policy and receipt projection around the mounted actor.
use crate::{
    mount_support::{refusal, MountError},
    TapeUuid, WriteObjectToPoolRequest,
};
use remanence_state::CatalogIndex;
use std::path::PathBuf;

/// A catalog-backed receipt; no physical writer report is invented for an actor append.
#[derive(Debug)]
pub struct MountedWriteResult {
    pub object: crate::PoolWriteObjectRecord,
    replay: bool,
}
impl MountedWriteResult {
    pub(crate) fn replayed(object: crate::PoolWriteObjectRecord) -> Self {
        Self {
            object,
            replay: true,
        }
    }
    pub fn is_replay(&self) -> bool {
        self.replay
    }
}
pub(crate) fn receipt(
    index: &CatalogIndex,
    record: crate::pb::ObjectRecord,
    pool: &str,
    replay: bool,
) -> Result<MountedWriteResult, MountError> {
    let id = uuid::Uuid::from_slice(&record.object_id)
        .map_err(refusal)?
        .to_string();
    let native = index
        .get_native_object(&id)
        .map_err(refusal)?
        .ok_or_else(|| refusal("committed receipt disappeared"))?;
    let object = crate::pool_write::pool_write_object_record_from_native(native, pool)
        .map_err(|e| MountError::PoolWrite(Box::new(e)))?;
    Ok(MountedWriteResult { object, replay })
}

pub(crate) fn prepare_version(
    index: &CatalogIndex,
    mut request: WriteObjectToPoolRequest,
    allow_plaintext_copy: bool,
    before_prepare: impl FnOnce(),
) -> Result<Result<(WriteObjectToPoolRequest, &'static str), remanence_state::VersionRow>, MountError>
{
    let path = remanence_stream::normalize_archive_path(&request.archive_path).map_err(refusal)?;
    request.archive_path = PathBuf::from(&path);
    if !allow_plaintext_copy
        && matches!(
            request.representation,
            crate::PoolWriteRepresentation::Plaintext
        )
    {
        if let Some(version) = index
            .list_committed_versions_by_path_any_pool(&path)
            .map_err(refusal)?
            .first()
        {
            let record = index
                .get_native_object(&version.object_id)
                .map_err(refusal)?
                .ok_or_else(|| refusal("version object disappeared"))?;
            if record
                .copies
                .iter()
                .any(|copy| copy.status == "committed" && copy.representation == "encrypted")
            {
                return Err(MountError::PlaintextCopy {
                    newest_version: version.object_id.clone(),
                });
            }
        }
    }
    let newest = index
        .list_committed_versions_by_path(&request.pool_id, &path)
        .map_err(refusal)?
        .into_iter()
        .next();
    let representation_matches = if let Some(version) = &newest {
        let record = index
            .get_native_object(&version.object_id)
            .map_err(refusal)?
            .ok_or_else(|| refusal("version object disappeared"))?;
        record.copies.iter().any(|copy| {
            copy.status == "committed"
                && copy.pool_id.as_deref() == Some(request.pool_id.as_str())
                && match &request.representation {
                    crate::PoolWriteRepresentation::Plaintext => copy.representation == "plaintext",
                    crate::PoolWriteRepresentation::Encrypted { recipients } => {
                        let requested: std::collections::BTreeSet<_> = recipients
                            .iter()
                            .map(|r| {
                                r.recipient_epoch_id
                                    .iter()
                                    .map(|b| format!("{b:02x}"))
                                    .collect::<String>()
                            })
                            .collect();
                        copy.representation == "encrypted"
                            && copy.recipient_epoch_ids.as_ref().is_some_and(|ids| {
                                ids.iter()
                                    .cloned()
                                    .collect::<std::collections::BTreeSet<_>>()
                                    == requested
                            })
                    }
                }
        })
    } else {
        false
    };
    let reason = if newest.is_none() {
        "new"
    } else if !representation_matches {
        "representation_changed"
    } else {
        "content_changed"
    };
    if let Some(version) = &newest {
        if representation_matches
            && request.source.size_bytes().map_err(refusal)? == version.size_bytes
            && request.source.content_sha256().map_err(refusal)?.as_slice() == version.file_sha256
        {
            return Ok(Err(version.clone()));
        }
    }
    let crate::WriteObjectSource::Path(source) = &request.source else {
        return Err(refusal("standalone put requires a regular file path"));
    };
    before_prepare();
    let prepared = remanence_stream::prepare_regular_file(
        source,
        &request.archive_path,
        uuid::Uuid::new_v4().to_string(),
    )
    .map_err(refusal)?;
    if let Some(version) = newest {
        if representation_matches
            && prepared.spec.size_bytes == version.size_bytes
            && prepared.spec.file_sha256.as_ref().map(|d| d.as_slice())
                == Some(version.file_sha256.as_slice())
        {
            return Ok(Err(version));
        }
    }
    request.source =
        crate::WriteObjectSource::PreparedPath(crate::pool_write::PreparedPathSource(prepared));
    Ok(Ok((request, reason)))
}

pub(crate) fn legacy_status(error: crate::engine::drive_adapter::ActorFailure) -> MountError {
    refusal(crate::write_owner::ActorFailure::without_local_cause(
        error.status,
    ))
}
pub(crate) fn supersede(
    client: &crate::engine::drive_adapter::MountedCommands,
    state: &mut remanence_state::StateHandle,
    acknowledge: &str,
    confirm: impl FnOnce(&str, &[String]) -> Result<(), String>,
) -> Result<Vec<String>, MountError> {
    let uuid = client.tape_uuid();
    let row = state
        .catalog_index()
        .get_tape(&uuid)
        .map_err(refusal)?
        .ok_or_else(|| refusal("adopt the tape before superseding its tail"))?;
    if acknowledge != uuid::Uuid::from_bytes(uuid).to_string()
        && row.voltag.as_deref() != Some(acknowledge)
    {
        return Err(refusal("acknowledgment does not identify the loaded tape"));
    }
    let mut request = crate::pb::SupersedeTailRequest {
        tape_uuid: uuid.to_vec(),
        ack_tape_uuid: uuid.to_vec(),
        confirm_supersede: None,
        expected_found: vec![],
    };
    let preview = client
        .supersede_tail(request.clone())
        .map_err(legacy_status)?;
    confirm(&preview.required_confirmation, &preview.found).map_err(refusal)?;
    request.confirm_supersede = Some(preview.required_confirmation);
    request.expected_found = preview.found;
    client
        .supersede_tail(request)
        .map(|reply| reply.found)
        .map_err(legacy_status)
}

/// Operator acknowledgements are checked before the shared finalization admission.
pub struct StandaloneFinalizeRequest {
    pub tape_uuid: TapeUuid,
    pub acknowledge_tape: TapeUuid,
    pub expected_pool_id: Option<String>,
    pub reason: String,
    pub idempotency_key: uuid::Uuid,
}
