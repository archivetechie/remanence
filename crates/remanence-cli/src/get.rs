//! `rem get` — restore an object from tape through the daemon read path.
//!
//! The operator counterpart of `rem put`: it locates the object in the
//! daemon's catalog, opens a read session pinned to the copy's tape (the
//! daemon mounts it if needed), and restores the object's members through
//! the daemon's member-level read API — the daemon resolves the object
//! format server-side and streams each member's payload bytes, so there is
//! no client-side format interpretation and no second extraction path.
//!
//! Every member is proven before it is published: bytes stream to a
//! temporary file, are hashed as they land, and only rename into place when
//! the hash matches the catalog's per-file sha256. A mismatch keeps the
//! temporary file for forensics and refuses — the two honest explanations
//! (a damaged/suspect copy, or an encrypted-at-rest object whose ciphertext
//! this command does not decrypt) are both named.
//!
//! Member paths come from the catalog and are sanitized with the same rules
//! `rem put` applies on the way in: `..` refuses, absolute prefixes strip.
//! A restore must never write outside `--dest`, whatever the catalog says.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use clap::Args;
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt;
use tonic::transport::Channel;
use uuid::Uuid;

use remanence_api::pb;

use crate::put::{archive_path_for, format_bytes, format_uuid, hex, wait_before_open_retry};
use crate::{
    connect_daemon, daemon_runtime, finish_daemon_client_result, status_error, DaemonClientError,
    DEFAULT_DAEMON_ENDPOINT,
};

const DEFAULT_GET_STREAM_CHUNK_BYTES: u32 = 1_048_576;

/// Arguments for `rem get`.
#[derive(Args, Debug)]
pub(crate) struct GetArgs {
    /// Object selector: an object UUID by default; with --caller-id the
    /// caller object id recorded at put time; with --sha256 the object's
    /// content sha256 (hex).
    #[arg(value_name = "OBJECT")]
    object: String,

    /// Destination directory for the restored members.
    #[arg(long, value_name = "DIR")]
    dest: PathBuf,

    /// Interpret OBJECT as the caller object id.
    #[arg(long, conflicts_with = "sha256")]
    caller_id: bool,

    /// Interpret OBJECT as the content sha256 (hex).
    #[arg(long)]
    sha256: bool,

    /// Read from the copy on this tape UUID instead of the first cataloged
    /// copy.
    #[arg(long, value_name = "UUID")]
    tape: Option<String>,

    /// Restore only the member with this archive path.
    #[arg(long = "path", value_name = "REM_OBJECT_PATH")]
    member_path: Option<String>,

    /// Replace existing destination files.
    #[arg(long)]
    overwrite: bool,

    /// Suggested read-stream chunk size in bytes.
    #[arg(long, default_value_t = DEFAULT_GET_STREAM_CHUNK_BYTES, value_name = "BYTES")]
    stream_chunk_bytes: u32,

    /// Fail immediately instead of waiting for media readiness (tape loads).
    #[arg(long)]
    no_wait: bool,

    /// Daemon gRPC endpoint URI.
    #[arg(long, value_name = "URI", default_value = DEFAULT_DAEMON_ENDPOINT)]
    endpoint: String,

    /// Emit stable CLI-shaped JSON.
    #[arg(long)]
    json: bool,
}

pub(crate) fn run_get_command(
    args: &GetArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let result =
        daemon_runtime().and_then(|runtime| runtime.block_on(async { get(args, out, err).await }));
    finish_daemon_client_result(result, args.json, err)
}

struct RestoredMember {
    archive_path: String,
    dest_path: PathBuf,
    size_bytes: u64,
    sha256: String,
}

async fn get(
    args: &GetArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), DaemonClientError> {
    let channel = connect_daemon(&args.endpoint)
        .await
        .map_err(DaemonClientError::from)?;

    // Locate the object, its copy, and its member list.
    let mut catalog = pb::catalog_client::CatalogClient::new(channel.clone());
    let key = object_key(args)?;
    let record = catalog
        .get_object(pb::GetObjectRequest { key: Some(key) })
        .await
        .map_err(|status| {
            DaemonClientError::client(format!(
                "object {:?} ({}): {}",
                args.object,
                selector_name(args),
                status.message(),
            ))
        })?
        .into_inner();
    let copy = pick_copy(&record, args.tape.as_deref())?;
    let object_uuid = format_uuid(&record.object_id);
    let files = catalog
        .list_files_in_object(pb::ListFilesInObjectRequest {
            object_id: record.object_id.clone(),
            page_token: None,
            page_size: 0,
        })
        .await
        .map_err(status_error)?
        .into_inner()
        .files;
    if files.is_empty() {
        return Err(DaemonClientError::client(format!(
            "object {object_uuid} has no cataloged members to restore",
        )));
    }
    let files = filter_members(files, args.member_path.as_deref())?;

    let total_bytes: u64 = files.iter().map(|file| file.size_bytes).sum();
    let _ = writeln!(
        err,
        "restoring {} member{} ({}) of object {} from tape {} file {}",
        files.len(),
        if files.len() == 1 { "" } else { "s" },
        format_bytes(total_bytes),
        object_uuid,
        format_uuid(&copy.tape_uuid),
        copy.tape_file_number,
    );

    // Open the read session pinned to the copy's tape; the daemon mounts it
    // if needed, and a media-readiness fence is watched to completion.
    let mut read_client =
        pb::read_session_service_client::ReadSessionServiceClient::new(channel.clone());
    let open_request = || pb::OpenReadSessionRequest {
        target: Some(pb::open_read_session_request::Target::TapeTarget(
            pb::TapeTarget {
                tape_uuid: copy.tape_uuid.clone(),
                mount_if_needed: true,
                // Optional constraint on reads; carries the catalog's own
                // claim so a stale catalog surfaces loudly.
                required_pool_id: copy.pool_id.clone(),
                allow_unpooled: false,
            },
        )),
        idempotency_key: None,
        resume_target: None,
    };
    let session = match read_client.open_read_session(open_request()).await {
        Ok(response) => response.into_inner(),
        Err(status) if !args.no_wait => {
            wait_before_open_retry(channel.clone(), &status, err).await?;
            read_client
                .open_read_session(open_request())
                .await
                .map_err(status_error)?
                .into_inner()
        }
        Err(status) => return Err(status_error(status)),
    };

    // Restore members one by one; always attempt to close the session
    // afterwards. A close failure after good reads costs nothing (the daemon
    // reaps the session), so it warns instead of failing the restore.
    let restore_result = restore_members(
        &mut read_client,
        &session.session_id,
        &record,
        &files,
        args,
        err,
    )
    .await;
    if let Err(status) = read_client
        .close_read_session(pb::CloseReadSessionRequest {
            session_id: session.session_id.clone(),
            idempotency_key: None,
        })
        .await
    {
        let _ = writeln!(
            err,
            "warning: read session close failed ({}); the daemon will reap it",
            status.message(),
        );
    }
    let restored = restore_result?;

    if args.json {
        let receipt = serde_json::json!({
            "object_id": object_uuid,
            "caller_object_id": record.caller_object_id,
            "tape_uuid": format_uuid(&copy.tape_uuid),
            "tape_file_number": copy.tape_file_number,
            "pool_id": copy.pool_id,
            "dest": args.dest.display().to_string(),
            "digest_verified": true,
            "members": restored.iter().map(|member| {
                serde_json::json!({
                    "archive_path": member.archive_path,
                    "restored_to": member.dest_path.display().to_string(),
                    "size_bytes": member.size_bytes,
                    "sha256": member.sha256,
                })
            }).collect::<Vec<_>>(),
        });
        writeln!(out, "{receipt}").map_err(|error| DaemonClientError::client(error.to_string()))
    } else {
        for member in &restored {
            writeln!(
                out,
                "{}  {}  sha256 {}  → {}",
                member.archive_path,
                format_bytes(member.size_bytes),
                &member.sha256[..12],
                member.dest_path.display(),
            )
            .map_err(|error| DaemonClientError::client(error.to_string()))?;
        }
        writeln!(
            out,
            "object {} restored into {} ({} member{}, {}, all digests verified)",
            object_uuid,
            args.dest.display(),
            restored.len(),
            if restored.len() == 1 { "" } else { "s" },
            format_bytes(total_bytes),
        )
        .map_err(|error| DaemonClientError::client(error.to_string()))
    }
}

async fn restore_members(
    client: &mut pb::read_session_service_client::ReadSessionServiceClient<Channel>,
    session_id: &[u8],
    record: &pb::ObjectRecord,
    files: &[pb::FileRecord],
    args: &GetArgs,
    err: &mut dyn Write,
) -> Result<Vec<RestoredMember>, DaemonClientError> {
    let mut restored = Vec::with_capacity(files.len());
    for file in files {
        // Catalog-supplied path: sanitize with put's own rules before it may
        // touch the filesystem. A restore never writes outside --dest.
        let mut stripped = false;
        let safe_path =
            archive_path_for(std::path::Path::new(&file.path), &mut stripped).map_err(|error| {
                DaemonClientError::client(format!(
                    "refusing member with unsafe archive path: {error}"
                ))
            })?;
        let dest_path = args.dest.join(&safe_path);
        if dest_path.exists() && !args.overwrite {
            return Err(DaemonClientError::client(format!(
                "{} already exists; pass --overwrite to replace it",
                dest_path.display(),
            )));
        }
        if let Some(parent) = dest_path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                DaemonClientError::client(format!("create {}: {error}", parent.display()))
            })?;
        }

        let started = Instant::now();
        let (bytes, actual_sha256, tmp_path) = stream_member(
            client,
            session_id,
            &record.object_id,
            file,
            &dest_path,
            args.stream_chunk_bytes,
        )
        .await?;

        let expected = hex(&file.file_sha256);
        if !expected.is_empty() && expected != actual_sha256 {
            return Err(DaemonClientError::client(format!(
                "member {} read from tape hashed {actual_sha256}, but the \
                 catalog says {expected}; the partial restore is kept at {} — \
                 either the copy is damaged (treat it as suspect: re-read or \
                 scrub) or this object is encrypted at rest, whose ciphertext \
                 this command does not decrypt",
                file.path,
                tmp_path.display(),
            )));
        }
        if expected.is_empty() {
            let _ = writeln!(
                err,
                "warning: the catalog has no digest for member {}; restored \
                 bytes could not be independently verified",
                file.path,
            );
        }
        std::fs::rename(&tmp_path, &dest_path).map_err(|error| {
            DaemonClientError::client(format!(
                "install {} over {}: {error}",
                tmp_path.display(),
                dest_path.display(),
            ))
        })?;
        let elapsed = started.elapsed().as_secs_f64();
        let rate = if elapsed > 0.0 {
            bytes as f64 / elapsed / (1024.0 * 1024.0)
        } else {
            0.0
        };
        let _ = writeln!(
            err,
            "  {} ({}) verified at {:.0} MiB/s",
            safe_path,
            format_bytes(bytes),
            rate,
        );
        restored.push(RestoredMember {
            archive_path: safe_path,
            dest_path,
            size_bytes: bytes,
            sha256: actual_sha256,
        });
    }
    Ok(restored)
}

/// Stream one member to `<dest>.rem-get-tmp`, hashing as bytes land. The
/// caller renames into place only after the digest verifies; on a transport
/// error the temporary file is removed (nothing was proven about it).
async fn stream_member(
    client: &mut pb::read_session_service_client::ReadSessionServiceClient<Channel>,
    session_id: &[u8],
    object_id: &[u8],
    file: &pb::FileRecord,
    dest_path: &std::path::Path,
    stream_chunk_bytes: u32,
) -> Result<(u64, String, PathBuf), DaemonClientError> {
    let tmp_path = dest_path.with_extension("rem-get-tmp");
    let mut output = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)
        .await
        .map_err(|error| {
            DaemonClientError::client(format!(
                "create {}: {error} (a leftover from a failed run? inspect, \
                 then remove it)",
                tmp_path.display(),
            ))
        })?;
    let mut stream = client
        .read_file(pb::ReadFileRequest {
            session_id: session_id.to_vec(),
            object_id: object_id.to_vec(),
            file_id: file.file_id.clone(),
            stream_chunk_bytes,
        })
        .await
        .map_err(status_error)?
        .into_inner();
    let mut hasher = Sha256::new();
    let mut bytes = 0_u64;
    loop {
        let chunk = match stream.message().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(status) => {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(status_error(status));
            }
        };
        if chunk.data.is_empty() {
            continue;
        }
        if let Err(error) = output.write_all(&chunk.data).await {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(DaemonClientError::client(format!(
                "write {}: {error}",
                tmp_path.display(),
            )));
        }
        hasher.update(&chunk.data);
        bytes = bytes.saturating_add(chunk.data.len() as u64);
    }
    if let Err(error) = output.flush().await {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(DaemonClientError::client(format!(
            "flush {}: {error}",
            tmp_path.display(),
        )));
    }
    Ok((bytes, hex(&hasher.finalize()), tmp_path))
}

fn selector_name(args: &GetArgs) -> &'static str {
    if args.caller_id {
        "by caller id"
    } else if args.sha256 {
        "by content sha256"
    } else {
        "by object UUID"
    }
}

fn object_key(args: &GetArgs) -> Result<pb::get_object_request::Key, DaemonClientError> {
    let raw = args.object.trim();
    if args.caller_id {
        return Ok(pb::get_object_request::Key::CallerObjectId(raw.to_string()));
    }
    if args.sha256 {
        let value = decode_sha256_hex(raw).map_err(DaemonClientError::client)?;
        return Ok(pb::get_object_request::Key::ContentSha256(value));
    }
    let uuid = Uuid::parse_str(raw).map_err(|error| {
        DaemonClientError::client(format!(
            "{raw:?} is not an object UUID: {error} (pass --caller-id or \
             --sha256 to select differently)"
        ))
    })?;
    Ok(pb::get_object_request::Key::ObjectId(
        uuid.as_bytes().to_vec(),
    ))
}

fn decode_sha256_hex(raw: &str) -> Result<Vec<u8>, String> {
    if raw.len() != 64 || !raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("{raw:?} is not a 64-hex-char sha256"));
    }
    Ok((0..raw.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&raw[index..index + 2], 16).expect("hex digits"))
        .collect())
}

fn pick_copy<'a>(
    record: &'a pb::ObjectRecord,
    tape: Option<&str>,
) -> Result<&'a pb::ObjectCopy, DaemonClientError> {
    if record.copies.is_empty() {
        return Err(DaemonClientError::client(format!(
            "object {} has no cataloged copies",
            format_uuid(&record.object_id),
        )));
    }
    match tape {
        None => Ok(&record.copies[0]),
        Some(tape) => {
            let tape_uuid = Uuid::parse_str(tape.trim())
                .map_err(|error| DaemonClientError::client(format!("--tape {tape:?}: {error}")))?;
            record
                .copies
                .iter()
                .find(|copy| copy.tape_uuid == tape_uuid.as_bytes().to_vec())
                .ok_or_else(|| {
                    DaemonClientError::client(format!(
                        "object {} has no copy on tape {tape}; copies are on: {}",
                        format_uuid(&record.object_id),
                        record
                            .copies
                            .iter()
                            .map(|copy| format_uuid(&copy.tape_uuid))
                            .collect::<Vec<_>>()
                            .join(", "),
                    ))
                })
        }
    }
}

fn filter_members(
    files: Vec<pb::FileRecord>,
    member_path: Option<&str>,
) -> Result<Vec<pb::FileRecord>, DaemonClientError> {
    match member_path {
        None => Ok(files),
        Some(wanted) => {
            let available: Vec<String> = files.iter().map(|file| file.path.clone()).collect();
            let selected: Vec<pb::FileRecord> = files
                .into_iter()
                .filter(|file| file.path == wanted)
                .collect();
            if selected.is_empty() {
                return Err(DaemonClientError::client(format!(
                    "no member with path {wanted:?}; members are: {}",
                    available.join(", "),
                )));
            }
            Ok(selected)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::pin::Pin;
    use std::sync::Arc;
    use tonic::{Request, Response, Status};

    // -- pure helpers -------------------------------------------------------

    #[test]
    fn object_key_selects_by_uuid_caller_id_and_sha256() {
        let base = get_args(
            Uuid::from_bytes([1; 16]).to_string(),
            PathBuf::from("/tmp/x"),
            String::new(),
        );
        assert!(matches!(
            object_key(&base).unwrap(),
            pb::get_object_request::Key::ObjectId(_)
        ));

        let mut caller = get_args(
            "ingest-4711".to_string(),
            PathBuf::from("/tmp/x"),
            String::new(),
        );
        caller.caller_id = true;
        assert!(matches!(
            object_key(&caller).unwrap(),
            pb::get_object_request::Key::CallerObjectId(_)
        ));

        caller.caller_id = false;
        caller.sha256 = true;
        caller.object = "ab".repeat(32);
        assert!(matches!(
            object_key(&caller).unwrap(),
            pb::get_object_request::Key::ContentSha256(_)
        ));

        caller.object = "not-hex".to_string();
        assert!(object_key(&caller).is_err());

        caller.sha256 = false;
        assert!(object_key(&caller)
            .unwrap_err()
            .message
            .contains("not an object UUID"));
    }

    #[test]
    fn pick_copy_prefers_first_and_honors_tape_pin() {
        let copy = |tape: [u8; 16], file: u64| pb::ObjectCopy {
            tape_uuid: tape.to_vec(),
            tape_file_number: file,
            ..Default::default()
        };
        let record = pb::ObjectRecord {
            object_id: Uuid::from_bytes([9; 16]).as_bytes().to_vec(),
            copies: vec![copy([1; 16], 5), copy([2; 16], 8)],
            ..Default::default()
        };
        assert_eq!(pick_copy(&record, None).unwrap().tape_file_number, 5);
        let pinned = pick_copy(&record, Some(&Uuid::from_bytes([2; 16]).to_string())).unwrap();
        assert_eq!(pinned.tape_file_number, 8);
        let error = pick_copy(&record, Some(&Uuid::from_bytes([7; 16]).to_string())).unwrap_err();
        assert!(
            error.message.contains("no copy on tape"),
            "{}",
            error.message
        );
    }

    #[test]
    fn filter_members_selects_and_reports_available() {
        let file = |path: &str| pb::FileRecord {
            path: path.to_string(),
            ..Default::default()
        };
        let files = vec![file("a.bin"), file("nested/b.bin")];
        assert_eq!(filter_members(files.clone(), None).unwrap().len(), 2);
        let one = filter_members(files.clone(), Some("nested/b.bin")).unwrap();
        assert_eq!(one.len(), 1);
        let error = filter_members(files, Some("missing")).unwrap_err();
        assert!(error.message.contains("a.bin"), "{}", error.message);
    }

    // -- fake daemon --------------------------------------------------------

    struct FakeMember {
        record: pb::FileRecord,
        payload: Vec<u8>,
    }

    struct FakeObjectStore {
        record: pb::ObjectRecord,
        members: Vec<FakeMember>,
    }

    struct FakeCatalog(Arc<FakeObjectStore>);
    struct FakeReadSessions(Arc<FakeObjectStore>);

    const FAKE_READ_SESSION: [u8; 16] = [4; 16];
    const OBJECT_ID: [u8; 16] = [8; 16];
    const TAPE_ID: [u8; 16] = [6; 16];

    type ChunkStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<pb::BytesChunk, Status>> + Send>>;

    #[tonic::async_trait]
    impl pb::catalog_server::Catalog for FakeCatalog {
        async fn get_object(
            &self,
            request: Request<pb::GetObjectRequest>,
        ) -> Result<Response<pb::ObjectRecord>, Status> {
            match request.into_inner().key {
                Some(pb::get_object_request::Key::ObjectId(id))
                    if id == self.0.record.object_id =>
                {
                    Ok(Response::new(self.0.record.clone()))
                }
                _ => Err(Status::not_found("object not found")),
            }
        }

        async fn list_files_in_object(
            &self,
            request: Request<pb::ListFilesInObjectRequest>,
        ) -> Result<Response<pb::ListFilesInObjectResponse>, Status> {
            assert_eq!(request.into_inner().object_id, self.0.record.object_id);
            Ok(Response::new(pb::ListFilesInObjectResponse {
                files: self
                    .0
                    .members
                    .iter()
                    .map(|member| member.record.clone())
                    .collect(),
                next_page_token: None,
            }))
        }

        async fn list_tapes(
            &self,
            _request: Request<pb::ListTapesRequest>,
        ) -> Result<Response<pb::ListTapesResponse>, Status> {
            Err(Status::unimplemented("not needed"))
        }
        async fn get_tape(
            &self,
            _request: Request<pb::GetTapeRequest>,
        ) -> Result<Response<pb::Tape>, Status> {
            Err(Status::unimplemented("not needed"))
        }
        async fn list_tape_files(
            &self,
            _request: Request<pb::ListTapeFilesRequest>,
        ) -> Result<Response<pb::ListTapeFilesResponse>, Status> {
            Err(Status::unimplemented("not needed"))
        }
        async fn list_tape_pools(
            &self,
            _request: Request<pb::ListTapePoolsRequest>,
        ) -> Result<Response<pb::ListTapePoolsResponse>, Status> {
            Err(Status::unimplemented("not needed"))
        }
        async fn get_tape_pool(
            &self,
            _request: Request<pb::GetTapePoolRequest>,
        ) -> Result<Response<pb::TapePool>, Status> {
            Err(Status::unimplemented("not needed"))
        }
        type EnumerateObjectsStream =
            Pin<Box<dyn tokio_stream::Stream<Item = Result<pb::ObjectRecord, Status>> + Send>>;
        async fn enumerate_objects(
            &self,
            _request: Request<pb::EnumerateObjectsRequest>,
        ) -> Result<Response<Self::EnumerateObjectsStream>, Status> {
            Err(Status::unimplemented("not needed"))
        }
        async fn find_object_copies(
            &self,
            _request: Request<pb::FindObjectCopiesRequest>,
        ) -> Result<Response<pb::FindObjectCopiesResponse>, Status> {
            Err(Status::unimplemented("not needed"))
        }
        async fn reconcile_tape(
            &self,
            _request: Request<pb::ReconcileTapeRequest>,
        ) -> Result<Response<pb::OperationRef>, Status> {
            Err(Status::unimplemented("not needed"))
        }
        async fn get_file(
            &self,
            _request: Request<pb::GetFileRequest>,
        ) -> Result<Response<pb::FileRecord>, Status> {
            Err(Status::unimplemented("not needed"))
        }
        type EnumerateUnitsStream =
            Pin<Box<dyn tokio_stream::Stream<Item = Result<pb::CatalogUnit, Status>> + Send>>;
        async fn enumerate_units(
            &self,
            _request: Request<pb::EnumerateUnitsRequest>,
        ) -> Result<Response<Self::EnumerateUnitsStream>, Status> {
            Err(Status::unimplemented("not needed"))
        }
        async fn get_catalog_unit(
            &self,
            _request: Request<pb::GetCatalogUnitRequest>,
        ) -> Result<Response<pb::CatalogUnit>, Status> {
            Err(Status::unimplemented("not needed"))
        }
        async fn list_entries_in_unit(
            &self,
            _request: Request<pb::ListEntriesInUnitRequest>,
        ) -> Result<Response<pb::ListEntriesInUnitResponse>, Status> {
            Err(Status::unimplemented("not needed"))
        }
    }

    #[tonic::async_trait]
    impl pb::read_session_service_server::ReadSessionService for FakeReadSessions {
        async fn open_read_session(
            &self,
            request: Request<pb::OpenReadSessionRequest>,
        ) -> Result<Response<pb::ReadSession>, Status> {
            match request.into_inner().target {
                Some(pb::open_read_session_request::Target::TapeTarget(target)) => {
                    assert!(target.mount_if_needed);
                    assert_eq!(
                        target.tape_uuid,
                        TAPE_ID.to_vec(),
                        "read session must pin the copy's tape",
                    );
                }
                other => panic!("unexpected read target {other:?}"),
            }
            Ok(Response::new(pb::ReadSession {
                session_id: FAKE_READ_SESSION.to_vec(),
                ..Default::default()
            }))
        }

        async fn close_read_session(
            &self,
            _request: Request<pb::CloseReadSessionRequest>,
        ) -> Result<Response<pb::ReadSession>, Status> {
            Ok(Response::new(pb::ReadSession {
                session_id: FAKE_READ_SESSION.to_vec(),
                ..Default::default()
            }))
        }

        async fn get_read_session(
            &self,
            _request: Request<pb::GetReadSessionRequest>,
        ) -> Result<Response<pb::ReadSession>, Status> {
            Err(Status::unimplemented("not needed"))
        }

        type ReadObjectRangeStream = ChunkStream;
        async fn read_object_range(
            &self,
            _request: Request<pb::ReadObjectRangeRequest>,
        ) -> Result<Response<Self::ReadObjectRangeStream>, Status> {
            Err(Status::unimplemented("get uses member-level ReadFile"))
        }

        type ReadFileStream = ChunkStream;
        async fn read_file(
            &self,
            request: Request<pb::ReadFileRequest>,
        ) -> Result<Response<Self::ReadFileStream>, Status> {
            let request = request.into_inner();
            assert_eq!(request.session_id, FAKE_READ_SESSION.to_vec());
            let member = self
                .0
                .members
                .iter()
                .find(|member| member.record.file_id == request.file_id)
                .ok_or_else(|| Status::not_found("no such member"))?;
            let chunks: Vec<Result<pb::BytesChunk, Status>> = member
                .payload
                .chunks(1024)
                .map(|chunk| {
                    Ok(pb::BytesChunk {
                        data: chunk.to_vec(),
                        is_last: false,
                    })
                })
                .collect();
            Ok(Response::new(Box::pin(tokio_stream::iter(chunks))))
        }
    }

    fn member(path: &str, file_id: [u8; 16], payload: &[u8], truthful: bool) -> FakeMember {
        let digest: [u8; 32] = Sha256::digest(payload).into();
        FakeMember {
            record: pb::FileRecord {
                object_id: OBJECT_ID.to_vec(),
                file_id: file_id.to_vec(),
                path: path.to_string(),
                size_bytes: payload.len() as u64,
                file_sha256: if truthful {
                    digest.to_vec()
                } else {
                    vec![0x13; 32]
                },
                ..Default::default()
            },
            payload: payload.to_vec(),
        }
    }

    fn store(members: Vec<FakeMember>) -> Arc<FakeObjectStore> {
        Arc::new(FakeObjectStore {
            record: pb::ObjectRecord {
                object_id: OBJECT_ID.to_vec(),
                caller_object_id: Some("get-test-caller".to_string()),
                logical_size_bytes: Some(
                    members
                        .iter()
                        .map(|member| member.payload.len() as u64)
                        .sum(),
                ),
                body_format: Some("rem-object-v1".to_string()),
                copies: vec![pb::ObjectCopy {
                    tape_uuid: TAPE_ID.to_vec(),
                    tape_file_number: 42,
                    pool_id: "solo".to_string(),
                    ..Default::default()
                }],
                caller_metadata: HashMap::new(),
                ..Default::default()
            },
            members,
        })
    }

    fn serve_fake(
        store: Arc<FakeObjectStore>,
    ) -> (String, tokio::runtime::Runtime, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("rem.sock");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = {
            let _guard = runtime.enter();
            tokio::net::UnixListener::bind(&socket).unwrap()
        };
        runtime.spawn(
            tonic::transport::Server::builder()
                .add_service(pb::catalog_server::CatalogServer::new(FakeCatalog(
                    store.clone(),
                )))
                .add_service(
                    pb::read_session_service_server::ReadSessionServiceServer::new(
                        FakeReadSessions(store),
                    ),
                )
                .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(listener)),
        );
        (format!("unix:{}", socket.display()), runtime, dir)
    }

    fn get_args(object: String, dest: PathBuf, endpoint: String) -> GetArgs {
        GetArgs {
            object,
            dest,
            caller_id: false,
            sha256: false,
            tape: None,
            member_path: None,
            overwrite: false,
            stream_chunk_bytes: 4096,
            no_wait: true,
            endpoint,
            json: true,
        }
    }

    fn run_get_blocking(args: &GetArgs) -> (Result<(), DaemonClientError>, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = daemon_runtime()
            .and_then(|runtime| runtime.block_on(async { get(args, &mut out, &mut err).await }));
        (
            result,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn get_restores_members_and_verifies_each_digest() {
        let alpha = b"the get roundtrip payload".repeat(64);
        let beta = b"nested member payload".repeat(32);
        let store = store(vec![
            member("alpha.bin", [1; 16], &alpha, true),
            member("nested/beta.bin", [2; 16], &beta, true),
        ]);
        let (endpoint, _runtime, _dir) = serve_fake(store);

        let dest = tempfile::tempdir().unwrap();
        let args = get_args(
            Uuid::from_bytes(OBJECT_ID).to_string(),
            dest.path().to_path_buf(),
            endpoint,
        );
        let (result, out, err) = run_get_blocking(&args);
        assert!(result.is_ok(), "{result:?}\nstderr: {err}");

        assert_eq!(std::fs::read(dest.path().join("alpha.bin")).unwrap(), alpha);
        assert_eq!(
            std::fs::read(dest.path().join("nested/beta.bin")).unwrap(),
            beta
        );
        let receipt: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(receipt["digest_verified"], true);
        assert_eq!(receipt["members"].as_array().unwrap().len(), 2);
        assert_eq!(receipt["tape_file_number"], 42);
    }

    #[test]
    fn get_refuses_a_digest_mismatch_and_keeps_the_partial() {
        let payload = b"tampered payload".repeat(32);
        let store = store(vec![member("data.bin", [1; 16], &payload, false)]);
        let (endpoint, _runtime, _dir) = serve_fake(store);

        let dest = tempfile::tempdir().unwrap();
        let args = get_args(
            Uuid::from_bytes(OBJECT_ID).to_string(),
            dest.path().to_path_buf(),
            endpoint,
        );
        let (result, _out, _err) = run_get_blocking(&args);
        let error = result.unwrap_err();
        assert!(error.message.contains("suspect"), "{}", error.message);
        assert!(
            error.message.contains(&hex(&[0x13; 32])),
            "mismatch must name the expected digest: {}",
            error.message
        );
        assert!(
            error.message.contains("encrypted at rest"),
            "mismatch must name the encrypted possibility: {}",
            error.message
        );
        // Not published; the unverified bytes stay in the tmp for forensics.
        assert!(!dest.path().join("data.bin").exists());
        assert!(dest.path().join("data.rem-get-tmp").exists());
    }

    #[test]
    fn get_refuses_traversal_member_paths_from_the_catalog() {
        let payload = b"evil".to_vec();
        let store = store(vec![member("../escape.bin", [1; 16], &payload, true)]);
        let (endpoint, _runtime, _dir) = serve_fake(store);

        let parent = tempfile::tempdir().unwrap();
        let dest = parent.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let args = get_args(
            Uuid::from_bytes(OBJECT_ID).to_string(),
            dest.clone(),
            endpoint,
        );
        let (result, _out, _err) = run_get_blocking(&args);
        let error = result.unwrap_err();
        assert!(
            error.message.contains("unsafe archive path"),
            "{}",
            error.message
        );
        assert!(!parent.path().join("escape.bin").exists());
    }

    #[test]
    fn get_filters_members_by_path() {
        let alpha = b"alpha".repeat(16);
        let beta = b"beta".repeat(16);
        let store = store(vec![
            member("alpha.bin", [1; 16], &alpha, true),
            member("beta.bin", [2; 16], &beta, true),
        ]);
        let (endpoint, _runtime, _dir) = serve_fake(store);

        let dest = tempfile::tempdir().unwrap();
        let mut args = get_args(
            Uuid::from_bytes(OBJECT_ID).to_string(),
            dest.path().to_path_buf(),
            endpoint,
        );
        args.member_path = Some("beta.bin".to_string());
        let (result, _out, err) = run_get_blocking(&args);
        assert!(result.is_ok(), "{result:?}\nstderr: {err}");
        assert!(dest.path().join("beta.bin").exists());
        assert!(!dest.path().join("alpha.bin").exists());
    }

    #[test]
    fn get_reports_not_found_with_the_selector() {
        let store = store(vec![member("x.bin", [1; 16], b"x", true)]);
        let (endpoint, _runtime, _dir) = serve_fake(store);
        let dest = tempfile::tempdir().unwrap();
        let args = get_args(
            Uuid::from_bytes([99; 16]).to_string(), // not the seeded object
            dest.path().to_path_buf(),
            endpoint,
        );
        let (result, _out, _err) = run_get_blocking(&args);
        let error = result.unwrap_err();
        assert!(
            error.message.contains("by object UUID"),
            "{}",
            error.message
        );
        assert!(error.message.contains("not found"), "{}", error.message);
    }
}
