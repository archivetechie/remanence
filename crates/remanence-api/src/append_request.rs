//! Write-session append validation, path resolution, and spool task adapters.

use std::io;
use std::path::PathBuf;

use tonic::Status;
use uuid::Uuid;

use crate::catalog_conversion::decode_uuid_bytes;
use crate::pb;

pub(crate) fn ensure_same_session(value: &[u8], expected: Uuid) -> Result<(), Status> {
    let actual = decode_uuid_bytes(value, "session_id")?;
    if Uuid::from_bytes(actual) == expected {
        Ok(())
    } else {
        Err(Status::invalid_argument(
            "append stream contains more than one session_id",
        ))
    }
}

pub(crate) fn expected_content_digest(
    legacy_sha256: Option<&[u8]>,
    digest: Option<&pb::Digest>,
) -> Result<Option<[u8; 32]>, Status> {
    // Supplied-and-empty is not "no digest": it is a caller that meant to bind
    // this append to a hash and sent nothing to bind it to.
    let legacy = legacy_sha256
        .map(|value| {
            value.try_into().map_err(|_| {
                Status::invalid_argument("expected_content_sha256 must be 32 bytes when supplied")
            })
        })
        .transpose()?;
    let paired = digest
        .map(|digest| {
            if digest.algorithm != remanence_state::DIGEST_ALGORITHM_SHA256 {
                return Err(Status::invalid_argument(
                    "expected_content_digest.algorithm must be sha256",
                ));
            }
            digest.value.as_slice().try_into().map_err(|_| {
                Status::invalid_argument("expected_content_digest.value must be 32 bytes")
            })
        })
        .transpose()?;
    if legacy.is_some() && paired.is_some() && legacy != paired {
        return Err(Status::invalid_argument(
            "expected_content_sha256 and expected_content_digest disagree",
        ));
    }
    Ok(paired.or(legacy))
}

pub(crate) fn overlap_append_eligible(
    mode: remanence_state::AppendStagingMode,
    start: &pb::AppendObjectStart,
    start_digest: Option<&[u8; 32]>,
) -> bool {
    mode == remanence_state::AppendStagingMode::Overlap
        && !start.caller_object_id.trim().is_empty()
        // An exact size, because the overlap receive path holds the stream to
        // it byte for byte. A declared zero is an empty object, which has
        // nothing to overlap; absent is a caller that does not know.
        && start.declared_size_bytes.is_some_and(|size| size != 0)
        && start_digest.is_some()
        && start.source_replay_capability == pb::SourceReplayCapability::ReplayFromStart as i32
        // Supplying an empty manifest is still supplying one, and manifests
        // are unwired -- so only their absence is eligible.
        && start.body_format_manifest.is_none()
}

/// Request-shape validation for a pinned-tape write target.
///
/// The pool guard is mandatory: pinning replaces pool *selection*, never
/// *admission*, and pools carry copy-class segregation — so the caller must
/// state which pool it believes the tape belongs to. Whether the guard is
/// *true* is checked later against the catalog (mount admission); this layer
/// only refuses requests that decline to state an intent at all.
pub(crate) fn validate_tape_target_shape(
    target: &pb::TapeTarget,
) -> Result<([u8; 16], String), Status> {
    let tape_uuid = decode_uuid_bytes(&target.tape_uuid, "tape_uuid")?;
    let required_pool_id = target.required_pool_id.trim().to_string();
    if target.allow_unpooled {
        if !required_pool_id.is_empty() {
            return Err(Status::invalid_argument(
                "allow_unpooled and required_pool_id are mutually exclusive",
            ));
        }
        return Err(Status::unimplemented(
            "unpooled tape targets are not wired in this slice; assign the \
             tape to a pool first",
        ));
    }
    if required_pool_id.is_empty() {
        return Err(Status::invalid_argument(
            "tape-target write sessions require required_pool_id naming the \
             tape's pool (pools carry copy-class segregation; the guard makes \
             the caller's intent checkable)",
        ));
    }
    if !target.mount_if_needed {
        return Err(Status::invalid_argument(
            "tape-target write sessions require mount_if_needed=true in this slice",
        ));
    }
    Ok((tape_uuid, required_pool_id))
}

pub(crate) fn archive_path_from_start(start: &pb::AppendObjectStart) -> PathBuf {
    start
        .caller_metadata
        .get("path")
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if start.caller_object_id.trim().is_empty() {
                PathBuf::from("payload.bin")
            } else {
                PathBuf::from(start.caller_object_id.clone())
            }
        })
}

/// Absent means the caller does not know the size, so the spool gets the full
/// cap. A declared size is a ceiling the caller has asked to be held to --
/// including a declared zero, which used to be indistinguishable from "unknown"
/// and silently bought the largest spool we allow.
pub(crate) fn append_spool_cap(declared_size_bytes: Option<u64>) -> u64 {
    declared_size_bytes.map_or(crate::write_owner::SPOOL_MAX_BYTES, |declared| {
        declared.min(crate::write_owner::SPOOL_MAX_BYTES)
    })
}

pub(crate) async fn create_append_spool(
    dir: PathBuf,
    cap: u64,
) -> Result<crate::write_owner::Spool, Status> {
    let dir_for_status = dir.clone();
    tokio::task::spawn_blocking(move || crate::write_owner::Spool::create(&dir, cap))
        .await
        .map_err(|err| Status::internal(format!("create append spool task failed: {err}")))?
        .map_err(|err| {
            Status::internal(format!(
                "create append spool in {}: {err}",
                dir_for_status.display()
            ))
        })
}

pub(crate) async fn write_append_spool_chunk(
    spool: crate::write_owner::Spool,
    data: Vec<u8>,
) -> Result<crate::write_owner::Spool, Status> {
    tokio::task::spawn_blocking(move || {
        let mut spool = spool;
        spool.write_chunk(&data).map(|()| spool)
    })
    .await
    .map_err(|err| Status::internal(format!("write append spool task failed: {err}")))?
    .map_err(status_from_append_spool_write_error)
}

pub(crate) async fn finish_append_spool(
    spool: crate::write_owner::Spool,
) -> Result<PathBuf, Status> {
    tokio::task::spawn_blocking(move || spool.finish())
        .await
        .map_err(|err| Status::internal(format!("finish append spool task failed: {err}")))?
        .map_err(|err| Status::internal(format!("finish append spool: {err}")))
}

pub(crate) fn status_from_append_spool_write_error(err: io::Error) -> Status {
    if err.kind() == io::ErrorKind::InvalidInput {
        Status::resource_exhausted(format!("object exceeds append spool size cap: {err}"))
    } else {
        Status::internal(format!("write append spool: {err}"))
    }
}
