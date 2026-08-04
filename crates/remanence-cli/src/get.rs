//! `rem get` — restore an object from tape through the daemon read path.
//!
//! The operator counterpart of `rem put`: it locates the object in the
//! daemon's catalog, opens a read session pinned to the copy's tape (the
//! daemon mounts it if needed), streams the stored bytes to a local spool
//! file, proves the spooled bytes against the catalog digest, and then
//! restores members through exactly the extraction machinery `rem restore`
//! uses on local object files. There is deliberately no second extraction
//! path: `get` fetches, and the one existing restore funnel interprets.
//!
//! The spool is the recovery point: if extraction fails (wrong key, bad
//! member path), the verified spool file is kept and named, so the fix
//! costs a re-run of extraction, never a re-read of tape.

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

use crate::put::{format_bytes, format_uuid, hex, wait_before_open_retry};
use crate::{
    connect_daemon, daemon_runtime, extract_archive_object_file, finish_daemon_client_result,
    parse_archive_byte_range, status_error, ArchiveByteRange, ArchiveExtractArgs,
    DaemonClientError, DEFAULT_DAEMON_ENDPOINT,
};

const DEFAULT_GET_STREAM_CHUNK_BYTES: u32 = 1_048_576;
// Fallback object block/chunk grid when the catalog cannot say (mirrors
// `rem restore`'s default). The real grid is derived from the copy's tape.
const FALLBACK_EXTRACT_CHUNK_SIZE: usize = 256 * 1024;

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

    /// Canonical REMP private-key file. Required for encrypted objects.
    #[arg(long, value_name = "PATH")]
    private_key: Option<PathBuf>,

    /// Archive member path for byte-range extraction (forwarded to the
    /// restore funnel).
    #[arg(long = "path", value_name = "REM_OBJECT_PATH")]
    member_path: Option<String>,

    /// Member byte range to extract, formatted as start:length.
    #[arg(long, value_name = "START:LEN", value_parser = parse_archive_byte_range)]
    range: Option<ArchiveByteRange>,

    /// First object-local BodyLba for --path, from the receipt or catalog.
    #[arg(long = "first-chunk-lba", value_name = "LBA")]
    first_chunk_lba: Option<u64>,

    /// Full member size for --path, from the receipt or catalog.
    #[arg(long = "file-size-bytes", value_name = "BYTES")]
    file_size_bytes: Option<u64>,

    /// Replace existing destination files.
    #[arg(long)]
    overwrite: bool,

    /// Keep `.remwrap.tar` and `.remwrap.idx` entries literal instead of
    /// unwrapping.
    #[arg(long)]
    no_unwrap: bool,

    /// Object block/chunk grid for extraction. Default: the block size the
    /// catalog records for the copy's tape, which is the grid the daemon
    /// wrote the object on.
    #[arg(long, value_name = "BYTES")]
    chunk_size: Option<usize>,

    /// Directory for the tape-read spool file (default: `.rem-get-spool`
    /// under --dest).
    #[arg(long, value_name = "DIR")]
    spool_dir: Option<PathBuf>,

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

async fn get(
    args: &GetArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), DaemonClientError> {
    let channel = connect_daemon(&args.endpoint)
        .await
        .map_err(DaemonClientError::from)?;

    // Locate the object and pick its copy.
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
    // The extraction grid is the tape's block size: the daemon wrote the
    // object gridded on it, and extraction addressed on any other grid fails.
    let chunk_size = match args.chunk_size {
        Some(chunk_size) => chunk_size,
        None => match catalog
            .get_tape(pb::GetTapeRequest {
                tape_uuid: copy.tape_uuid.clone(),
            })
            .await
        {
            Ok(response) => match usize::try_from(response.into_inner().block_size_bytes) {
                Ok(block_size) if block_size > 0 => block_size,
                _ => FALLBACK_EXTRACT_CHUNK_SIZE,
            },
            Err(status) => {
                let _ = writeln!(
                    err,
                    "warning: could not read the tape's block size ({}); \
                     assuming {FALLBACK_EXTRACT_CHUNK_SIZE} — pass --chunk-size \
                     if extraction fails",
                    status.message(),
                );
                FALLBACK_EXTRACT_CHUNK_SIZE
            }
        },
    };
    let _ = writeln!(
        err,
        "reading object {} ({}) from tape {} file {}",
        object_uuid,
        format_bytes(record.logical_size_bytes),
        format_uuid(&copy.tape_uuid),
        copy.tape_file_number,
    );

    // Open the read session pinned to that copy's tape; the daemon mounts it
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

    // Stream the stored bytes to the spool, hashing as they land.
    let spool_dir = args
        .spool_dir
        .clone()
        .unwrap_or_else(|| args.dest.join(".rem-get-spool"));
    std::fs::create_dir_all(&spool_dir).map_err(|error| {
        DaemonClientError::client(format!("create {}: {error}", spool_dir.display()))
    })?;
    let spool_path = spool_dir.join(format!("get-{object_uuid}.rem"));
    let read_started = Instant::now();
    let read_result = stream_object_to_spool(
        &mut read_client,
        &session.session_id,
        &record.object_id,
        args.stream_chunk_bytes,
        &spool_path,
    )
    .await;
    // Always try to close; a close failure after a good read costs the
    // operator nothing (the daemon reaps the session), so it warns.
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
    let (body_bytes, body_sha256) = read_result?;
    let elapsed = read_started.elapsed().as_secs_f64();
    let rate = if elapsed > 0.0 {
        body_bytes as f64 / elapsed / (1024.0 * 1024.0)
    } else {
        0.0
    };
    let _ = writeln!(
        err,
        "read {} off tape at {:.0} MiB/s",
        format_bytes(body_bytes),
        rate,
    );

    // Prove the spooled bytes before interpreting them. The stored digest is
    // the on-tape representation's hash (what we just streamed); the content
    // digest covers the same bytes for plaintext objects.
    let verification = verify_body_digest(&record, copy, &body_sha256);
    if let DigestVerification::Mismatch { expected, source } = &verification {
        return Err(DaemonClientError::client(format!(
            "object {object_uuid} read from tape {} hashed {body_sha256}, but the \
             catalog {source} says {expected}; the spool is kept at {} — do not \
             trust it, and treat the copy as suspect (re-read or scrub)",
            format_uuid(&copy.tape_uuid),
            spool_path.display(),
        )));
    }
    if matches!(verification, DigestVerification::Unavailable) {
        let _ = writeln!(
            err,
            "warning: the catalog carries no digest for this object; the \
             spooled bytes could not be independently verified",
        );
    }

    // Restore through the same funnel `rem restore` uses on local files.
    let extract_args = ArchiveExtractArgs {
        object: spool_path.clone(),
        dest: args.dest.clone(),
        chunk_size,
        private_key: args.private_key.clone(),
        path: args.member_path.clone(),
        first_chunk_lba: args.first_chunk_lba,
        file_size_bytes: args.file_size_bytes,
        range: args.range,
        overwrite: args.overwrite,
        xattr_namespaces: Vec::new(),
        no_unwrap: args.no_unwrap,
        blob_entry: None,
        blob_member: None,
    };
    let restore_report = match extract_archive_object_file(&extract_args) {
        Ok(report) => report,
        Err(error) => {
            return Err(DaemonClientError::client(format!(
                "the object was read and verified, but extraction failed: {error}; \
                 the verified spool is kept at {} — fix the extraction inputs and \
                 re-run `rem restore --object` on it instead of re-reading tape",
                spool_path.display(),
            )));
        }
    };
    let _ = std::fs::remove_file(&spool_path);
    let _ = std::fs::remove_dir(&spool_dir); // only removes it when empty

    if args.json {
        let receipt = serde_json::json!({
            "object_id": object_uuid,
            "caller_object_id": record.caller_object_id,
            "tape_uuid": format_uuid(&copy.tape_uuid),
            "tape_file_number": copy.tape_file_number,
            "pool_id": copy.pool_id,
            "body_bytes": body_bytes,
            "body_sha256": body_sha256,
            "digest_verified": matches!(verification, DigestVerification::Verified),
            "dest": args.dest.display().to_string(),
            "restore": restore_report,
        });
        writeln!(out, "{receipt}").map_err(|error| DaemonClientError::client(error.to_string()))
    } else {
        writeln!(
            out,
            "object {} restored into {} ({}{})",
            object_uuid,
            args.dest.display(),
            format_bytes(body_bytes),
            if matches!(verification, DigestVerification::Verified) {
                ", digest verified"
            } else {
                ", digest UNVERIFIED"
            },
        )
        .map_err(|error| DaemonClientError::client(error.to_string()))
    }
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

async fn stream_object_to_spool(
    client: &mut pb::read_session_service_client::ReadSessionServiceClient<Channel>,
    session_id: &[u8],
    object_id: &[u8],
    stream_chunk_bytes: u32,
    spool_path: &std::path::Path,
) -> Result<(u64, String), DaemonClientError> {
    let mut spool = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(spool_path)
        .await
        .map_err(|error| {
            DaemonClientError::client(format!(
                "create spool {}: {error} (a leftover spool from a failed run? \
                 inspect it, then remove it)",
                spool_path.display(),
            ))
        })?;
    let mut stream = client
        .read_object_range(pb::ReadObjectRangeRequest {
            session_id: session_id.to_vec(),
            object_id: object_id.to_vec(),
            file_id: Vec::new(),
            start_byte: 0,
            end_byte: 0,
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
                let _ = std::fs::remove_file(spool_path);
                return Err(status_error(status));
            }
        };
        if chunk.data.is_empty() {
            continue;
        }
        if let Err(error) = spool.write_all(&chunk.data).await {
            let _ = std::fs::remove_file(spool_path);
            return Err(DaemonClientError::client(format!(
                "write spool {}: {error}",
                spool_path.display(),
            )));
        }
        hasher.update(&chunk.data);
        bytes = bytes.saturating_add(chunk.data.len() as u64);
    }
    if let Err(error) = spool.flush().await {
        let _ = std::fs::remove_file(spool_path);
        return Err(DaemonClientError::client(format!(
            "flush spool {}: {error}",
            spool_path.display(),
        )));
    }
    Ok((bytes, hex(&hasher.finalize())))
}

enum DigestVerification {
    Verified,
    Unavailable,
    Mismatch {
        expected: String,
        source: &'static str,
    },
}

/// Compare the streamed bytes against the catalog's claim. The per-copy
/// stored digest describes exactly the representation on tape, so it wins;
/// the object content digest covers the same bytes for plaintext objects.
fn verify_body_digest(
    record: &pb::ObjectRecord,
    copy: &pb::ObjectCopy,
    body_sha256: &str,
) -> DigestVerification {
    let candidates = [
        (
            "stored digest",
            copy.stored_digest
                .as_ref()
                .filter(|digest| digest.algorithm == "sha256")
                .map(|digest| hex(&digest.value)),
        ),
        (
            "content digest",
            (!record.content_sha256.is_empty()).then(|| hex(&record.content_sha256)),
        ),
    ];
    for (source, expected) in candidates {
        if let Some(expected) = expected {
            return if expected == body_sha256 {
                DigestVerification::Verified
            } else {
                DigestVerification::Mismatch { expected, source }
            };
        }
    }
    DigestVerification::Unavailable
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
        let base = GetArgs {
            object: Uuid::from_bytes([1; 16]).to_string(),
            dest: PathBuf::from("/tmp/x"),
            caller_id: false,
            sha256: false,
            tape: None,
            private_key: None,
            member_path: None,
            range: None,
            first_chunk_lba: None,
            file_size_bytes: None,
            overwrite: false,
            no_unwrap: false,
            chunk_size: None,
            spool_dir: None,
            stream_chunk_bytes: DEFAULT_GET_STREAM_CHUNK_BYTES,
            no_wait: true,
            endpoint: String::new(),
            json: true,
        };
        assert!(matches!(
            object_key(&base).unwrap(),
            pb::get_object_request::Key::ObjectId(_)
        ));

        let mut caller = GetArgs { ..base };
        caller.object = "ingest-4711".to_string();
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
        assert!(error.message.contains("no copy on tape"), "{}", error.message);

        let empty = pb::ObjectRecord {
            object_id: Uuid::from_bytes([9; 16]).as_bytes().to_vec(),
            ..Default::default()
        };
        assert!(pick_copy(&empty, None)
            .unwrap_err()
            .message
            .contains("no cataloged copies"));
    }

    // -- fake daemon --------------------------------------------------------

    struct FakeObjectStore {
        record: pb::ObjectRecord,
        body: Vec<u8>,
    }

    struct FakeCatalog(Arc<FakeObjectStore>);
    struct FakeReadSessions(Arc<FakeObjectStore>);

    const FAKE_READ_SESSION: [u8; 16] = [4; 16];

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

        async fn list_tapes(
            &self,
            _request: Request<pb::ListTapesRequest>,
        ) -> Result<Response<pb::ListTapesResponse>, Status> {
            Err(Status::unimplemented("not needed"))
        }
        async fn get_tape(
            &self,
            request: Request<pb::GetTapeRequest>,
        ) -> Result<Response<pb::Tape>, Status> {
            // The extraction grid derives from here: the fake's body is
            // built on a 4096 grid, so a hardcoded-grid regression fails.
            assert_eq!(request.into_inner().tape_uuid, [6; 16].to_vec());
            Ok(Response::new(pb::Tape {
                tape_uuid: [6; 16].to_vec(),
                voltag: "GETT01L9".to_string(),
                block_size_bytes: 4096,
                ..Default::default()
            }))
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
        async fn list_files_in_object(
            &self,
            _request: Request<pb::ListFilesInObjectRequest>,
        ) -> Result<Response<pb::ListFilesInObjectResponse>, Status> {
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
                        self.0.record.copies[0].tape_uuid,
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
            request: Request<pb::ReadObjectRangeRequest>,
        ) -> Result<Response<Self::ReadObjectRangeStream>, Status> {
            let request = request.into_inner();
            assert_eq!(request.session_id, FAKE_READ_SESSION.to_vec());
            assert_eq!(request.object_id, self.0.record.object_id);
            assert_eq!((request.start_byte, request.end_byte), (0, 0));
            let chunks: Vec<Result<pb::BytesChunk, Status>> = self
                .0
                .body
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

        type ReadFileStream = ChunkStream;
        async fn read_file(
            &self,
            _request: Request<pb::ReadFileRequest>,
        ) -> Result<Response<Self::ReadFileStream>, Status> {
            Err(Status::unimplemented("not needed"))
        }
    }

    /// Build a real plaintext REM-OBJECT body holding one member, so the
    /// extraction leg runs the genuine restore funnel, not a mock.
    fn build_body(member_path: &str, payload: &[u8]) -> Vec<u8> {
        use remanence_format::{RemTarFile, RemTarObjectOptions};
        let mut opts = RemTarObjectOptions::new(
            "11111111-2222-3333-4444-555555555555",
            "get-test-caller",
            "2026-08-04T00:00:00Z",
            "66666666-7777-8888-9999-aaaaaaaaaaaa",
        );
        opts.chunk_size = 4096;
        let files = [RemTarFile {
            path: member_path,
            file_id: "member-1",
            data: payload,
            mtime: Some("0"),
            executable: Some(false),
        }];
        let mut sink = remanence_library::VecBlockSink::new();
        remanence_format::write_rem_tar_object(&mut sink, &opts, &files)
            .expect("write test object");
        sink.blocks.concat()
    }

    fn store(body: Vec<u8>, content_sha256: Vec<u8>) -> Arc<FakeObjectStore> {
        Arc::new(FakeObjectStore {
            record: pb::ObjectRecord {
                object_id: Uuid::from_bytes([8; 16]).as_bytes().to_vec(),
                caller_object_id: "get-test-caller".to_string(),
                content_sha256,
                logical_size_bytes: body.len() as u64,
                body_format: "rem-object-v1".to_string(),
                copies: vec![pb::ObjectCopy {
                    tape_uuid: [6; 16].to_vec(),
                    tape_file_number: 42,
                    pool_id: "solo".to_string(),
                    ..Default::default()
                }],
                caller_metadata: HashMap::new(),
                ..Default::default()
            },
            body,
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
            private_key: None,
            member_path: None,
            range: None,
            first_chunk_lba: None,
            file_size_bytes: None,
            overwrite: false,
            no_unwrap: false,
            chunk_size: None,
            spool_dir: None,
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
    fn get_restores_a_member_and_verifies_the_digest() {
        let payload = b"the get roundtrip payload".repeat(64);
        let body = build_body("photos/pic.bin", &payload);
        let body_digest: [u8; 32] = Sha256::digest(&body).into();
        let store = store(body, body_digest.to_vec());
        let (endpoint, _runtime, _dir) = serve_fake(store.clone());

        let dest = tempfile::tempdir().unwrap();
        let args = get_args(
            Uuid::from_bytes([8; 16]).to_string(),
            dest.path().to_path_buf(),
            endpoint,
        );
        let (result, out, err) = run_get_blocking(&args);
        assert!(result.is_ok(), "{result:?}\nstderr: {err}");

        let restored = dest.path().join("photos/pic.bin");
        let bytes = std::fs::read(&restored).expect("restored member exists");
        assert_eq!(bytes, payload, "restored bytes must equal the original");

        let receipt: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(receipt["digest_verified"], true);
        assert_eq!(receipt["tape_file_number"], 42);
        // Spool cleaned up on success.
        assert!(!dest.path().join(".rem-get-spool").exists());
    }

    #[test]
    fn get_refuses_a_digest_mismatch_and_keeps_the_spool() {
        let payload = b"tampered payload".repeat(32);
        let body = build_body("data.bin", &payload);
        let store = store(body, vec![0x13; 32]); // catalog claims a different hash
        let (endpoint, _runtime, _dir) = serve_fake(store.clone());

        let dest = tempfile::tempdir().unwrap();
        let args = get_args(
            Uuid::from_bytes([8; 16]).to_string(),
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
        // Nothing restored, spool kept for forensics.
        assert!(!dest.path().join("data.bin").exists());
        assert!(dest
            .path()
            .join(".rem-get-spool")
            .join(format!("get-{}.rem", Uuid::from_bytes([8; 16])))
            .exists());
    }

    #[test]
    fn get_reports_not_found_with_the_selector() {
        let store = store(Vec::new(), Vec::new());
        let (endpoint, _runtime, _dir) = serve_fake(store);
        let dest = tempfile::tempdir().unwrap();
        let args = get_args(
            Uuid::from_bytes([99; 16]).to_string(), // not the seeded object
            dest.path().to_path_buf(),
            endpoint,
        );
        let (result, _out, _err) = run_get_blocking(&args);
        let error = result.unwrap_err();
        assert!(error.message.contains("by object UUID"), "{}", error.message);
        assert!(error.message.contains("not found"), "{}", error.message);
    }
}
