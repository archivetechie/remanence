//! `rem put` — archive local files onto tape through the daemon write path.
//!
//! This is the operator-facing counterpart of the orchestrator write path. It
//! drives exactly the same Layer 5 session RPCs (`OpenWriteSession` →
//! `AppendObject` → `CheckpointSession` → `CloseWriteSession`) over the local
//! socket, so pool policy, capacity reserve, media-readiness fences, and
//! checkpoint durability all apply unchanged. There is deliberately no second,
//! daemon-less write path: a put that bypassed the daemon would also bypass
//! every safety the daemon exists to enforce.
//!
//! Granularity follows the wired API slice: one input file becomes one object
//! whose single member carries the file's archive path (the same mapping the
//! orchestrator uses). Directories are walked like `tar`: `rem put photos/`
//! archives `photos/<...>` members, one object each.
//!
//! The receipt printed at the end comes from the daemon's checkpoint response
//! (with a catalog lookup as fallback), never from the append acknowledgement
//! alone: an object is only reported with a tape position once the daemon has
//! committed it.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::Args;
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncReadExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use uuid::Uuid;

use remanence_api::pb;

use crate::{
    connect_daemon, daemon_runtime, finish_daemon_client_result, parse_element_addr, status_error,
    DaemonClientError, DEFAULT_DAEMON_ENDPOINT,
};

const DEFAULT_PUT_CHUNK_BYTES: usize = 1_048_576;
const MAX_PUT_CHUNK_BYTES: usize = 64 * 1024 * 1024;
// Media-readiness waits mirror remfield-io: a cold library may need minutes
// of load/position work before the first open succeeds.
const DEFAULT_READY_TIMEOUT_SECONDS: u32 = 9_000;
const DEFAULT_READY_POLL_SECONDS: u32 = 30;
const DRIVE_BUSY_RETRY_DELAY: Duration = Duration::from_secs(1);

/// Arguments for `rem put`.
#[derive(Args, Debug)]
pub(crate) struct PutArgs {
    /// Files or directories to archive. Directories are walked recursively;
    /// each regular file becomes one object.
    #[arg(required = true, value_name = "PATH")]
    paths: Vec<PathBuf>,

    /// Target tape pool id. When omitted and exactly one pool is configured,
    /// that pool is used. With --tape it is the mandatory pool guard: the
    /// pool you believe that tape belongs to.
    #[arg(long, value_name = "POOL_ID", conflicts_with = "drive")]
    pool: Option<String>,

    /// Pin a specific tape by UUID; the daemon mounts it into a free drive
    /// if needed. Requires --pool naming the tape's pool — pinning replaces
    /// pool selection, never admission, and a pool mismatch is a refusal.
    #[arg(long, value_name = "UUID", conflicts_with = "drive", requires = "pool")]
    tape: Option<String>,

    /// Target drive element address (accepts `0x0100` or decimal). Writes to
    /// whatever tape that drive currently holds. (The daemon currently wires
    /// pool targets only and rejects this at open; the flag is ahead of that
    /// slice.)
    #[arg(long, value_name = "ELEMENT", value_parser = parse_element_addr)]
    drive: Option<u16>,

    /// Library serial. Optional constraint with --pool; required with --drive
    /// when more than one library is attached.
    #[arg(long, value_name = "SERIAL", conflicts_with = "tape")]
    library: Option<String>,

    /// Caller object id recorded in the catalog. Only valid with a single
    /// input file; the default is the file's archive path.
    #[arg(long, value_name = "ID")]
    id: Option<String>,

    /// Extra caller metadata as key=value. Repeatable. The `path` key is
    /// reserved (it carries the member's archive path).
    #[arg(long, value_name = "KEY=VALUE")]
    meta: Vec<String>,

    /// Append chunk size in bytes.
    #[arg(long, default_value_t = DEFAULT_PUT_CHUNK_BYTES, value_name = "BYTES")]
    chunk_bytes: usize,

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

pub(crate) fn run_put_command(
    args: &PutArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let result =
        daemon_runtime().and_then(|runtime| runtime.block_on(async { put(args, out, err).await }));
    finish_daemon_client_result(result, args.json, err)
}

/// One file scheduled for archiving.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PutInput {
    fs_path: PathBuf,
    archive_path: String,
    size_bytes: u64,
}

/// The resolved write target, one-to-one with the proto `oneof`.
enum PutTarget {
    Pool {
        pool_id: String,
        library_uuid: Vec<u8>,
    },
    Tape {
        tape_uuid: Vec<u8>,
        required_pool_id: String,
    },
    Drive {
        library_uuid: Vec<u8>,
        element: u16,
    },
}

impl PutTarget {
    fn to_proto(&self) -> pb::open_write_session_request::Target {
        match self {
            Self::Pool {
                pool_id,
                library_uuid,
            } => pb::open_write_session_request::Target::PoolTarget(pb::TapePoolTarget {
                pool_id: pool_id.clone(),
                library_uuid: library_uuid.clone(),
                mount_if_needed: true,
            }),
            Self::Tape {
                tape_uuid,
                required_pool_id,
            } => pb::open_write_session_request::Target::TapeTarget(pb::TapeTarget {
                tape_uuid: tape_uuid.clone(),
                mount_if_needed: true,
                required_pool_id: required_pool_id.clone(),
                allow_unpooled: false,
            }),
            Self::Drive {
                library_uuid,
                element,
            } => pb::open_write_session_request::Target::DriveTarget(pb::DriveTarget {
                library_uuid: library_uuid.clone(),
                drive_element_address: u32::from(*element),
                required_pool_id: String::new(),
            }),
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Pool { pool_id, .. } => format!("pool {pool_id}"),
            Self::Tape {
                tape_uuid,
                required_pool_id,
            } => format!("tape {} (pool {required_pool_id})", format_uuid(tape_uuid)),
            Self::Drive { element, .. } => format!("drive 0x{element:04x}"),
        }
    }
}

async fn put(
    args: &PutArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), DaemonClientError> {
    if args.chunk_bytes == 0 || args.chunk_bytes > MAX_PUT_CHUNK_BYTES {
        return Err(DaemonClientError::client(format!(
            "--chunk-bytes must be between 1 and {MAX_PUT_CHUNK_BYTES}"
        )));
    }

    let mut warnings = Vec::new();
    let inputs = collect_inputs(&args.paths, &mut warnings).map_err(DaemonClientError::client)?;
    for warning in &warnings {
        let _ = writeln!(err, "warning: {warning}");
    }
    if let Some(id) = &args.id {
        if inputs.len() != 1 {
            return Err(DaemonClientError::client(format!(
                "--id {id:?} needs exactly one input file; got {}",
                inputs.len()
            )));
        }
    }
    let extra_meta = parse_meta(&args.meta).map_err(DaemonClientError::client)?;

    let channel = connect_daemon(&args.endpoint)
        .await
        .map_err(DaemonClientError::from)?;
    let target = resolve_target(args, channel.clone()).await?;

    let total_bytes: u64 = inputs.iter().map(|input| input.size_bytes).sum();
    let _ = writeln!(
        err,
        "writing {} file{} ({}) to {}",
        inputs.len(),
        if inputs.len() == 1 { "" } else { "s" },
        format_bytes(total_bytes),
        target.describe(),
    );

    let mut client =
        pb::write_session_service_client::WriteSessionServiceClient::new(channel.clone());
    let session_id =
        open_write_session(channel.clone(), &mut client, &target, args.no_wait, err).await?;

    // Append every file; abort the whole session on the first failure rather
    // than leaving it open for the daemon to declare orphaned later.
    let mut appended: Vec<(PutInput, pb::ObjectRecord)> = Vec::with_capacity(inputs.len());
    for input in inputs {
        let caller_object_id = args
            .id
            .clone()
            .unwrap_or_else(|| input.archive_path.clone());
        let started = Instant::now();
        let record = match append_one(
            &mut client,
            &session_id,
            &input,
            &caller_object_id,
            &extra_meta,
            args.chunk_bytes,
        )
        .await
        {
            Ok(record) => record,
            Err(error) => {
                // Precision matters in both directions here: a successful
                // abort discards appends the daemon has not checkpointed
                // (though an automatic checkpoint may already have committed
                // earlier objects), while a FAILED abort leaves the session
                // open with its appends recoverable. Claiming "discarded"
                // after a failed abort would invite destructive recovery.
                let disposition = if abort_write_session(&mut client, &session_id, err).await {
                    "session aborted — objects not yet checkpointed by the \
                     daemon were discarded (check `rem archive list` for any \
                     that were)"
                } else {
                    "the abort also failed, so the session was left open; the \
                     daemon will declare it orphaned and its appends remain \
                     recoverable"
                };
                return Err(DaemonClientError::client(format!(
                    "append {} failed: {}; {disposition}",
                    input.archive_path, error.message
                )));
            }
        };
        let elapsed = started.elapsed().as_secs_f64();
        let rate = if elapsed > 0.0 {
            input.size_bytes as f64 / elapsed / (1024.0 * 1024.0)
        } else {
            0.0
        };
        let _ = writeln!(
            err,
            "  {} ({}) spooled at {:.0} MiB/s, pending checkpoint",
            input.archive_path,
            format_bytes(input.size_bytes),
            rate,
        );
        appended.push((input, record));
    }

    // Durability barrier: the checkpoint response is the daemon's statement of
    // what is actually committed on tape.
    // A failure past this point leaves the session open rather than aborting
    // it: the appended data is spooled or partially committed, and abort
    // would discard it. The daemon will declare the session orphaned, and
    // orphaned sessions are recoverable.
    let session_context = |stage: &str, error: DaemonClientError| {
        DaemonClientError::client(format!(
            "{stage} failed for session {}: {}; the session was left open and \
             the daemon will declare it orphaned (recoverable)",
            format_uuid(&session_id),
            error.message,
        ))
    };
    let checkpoint = client
        .checkpoint_session(pb::CheckpointSessionRequest {
            session_id: session_id.clone(),
            idempotency_key: None,
        })
        .await
        .map_err(|status| session_context("checkpoint", status_error(status)))?
        .into_inner();
    let session = client
        .close_write_session(pb::CloseWriteSessionRequest {
            session_id: session_id.clone(),
            idempotency_key: None,
        })
        .await
        .map_err(|status| session_context("close", status_error(status)))?
        .into_inner();

    let mut committed: HashMap<Vec<u8>, pb::ObjectRecord> = checkpoint
        .committed_objects
        .into_iter()
        .map(|object| (object.object_id.clone(), object))
        .collect();

    // An auto-checkpoint may have committed early objects before our explicit
    // barrier; those are absent from the barrier's response, so fall back to
    // the catalog rather than reporting them uncommitted. Verification
    // failures accumulate instead of aborting the loop: one failed lookup
    // must not discard the receipts of every object that IS confirmed. The
    // two failure kinds stay distinct — "the catalog call failed" leaves the
    // object's fate unknown, "no committed copy" is the daemon saying it is
    // not on tape.
    let total_appended = appended.len();
    let mut catalog = pb::catalog_client::CatalogClient::new(channel.clone());
    let mut receipts = Vec::with_capacity(appended.len());
    let mut unverified = Vec::new();
    for (input, record) in appended {
        let object = match committed.remove(&record.object_id) {
            Some(object) => object,
            None => match catalog
                .get_object(pb::GetObjectRequest {
                    key: Some(pb::get_object_request::Key::ObjectId(
                        record.object_id.clone(),
                    )),
                })
                .await
            {
                Ok(response) => response.into_inner(),
                Err(status) => {
                    unverified.push(format!(
                        "{} (object {}): commit state unknown — catalog \
                         lookup failed: {}",
                        input.archive_path,
                        format_uuid(&record.object_id),
                        status.message(),
                    ));
                    continue;
                }
            },
        };
        if object.copies.is_empty() {
            unverified.push(format!(
                "{} (object {}): daemon reports no committed copy",
                input.archive_path,
                format_uuid(&record.object_id),
            ));
            continue;
        }
        receipts.push((input, object));
    }

    let voltags = resolve_voltags(&mut catalog, &receipts).await;
    print_receipt(args, &target, &session, &receipts, &voltags, out)
        .map_err(DaemonClientError::from)?;
    if !unverified.is_empty() {
        return Err(DaemonClientError::client(format!(
            "{} of {} objects confirmed committed (receipts above); {} not \
             confirmed: {}",
            receipts.len(),
            total_appended,
            unverified.len(),
            unverified.join("; "),
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Input collection

/// Walk the CLI paths into a deterministic, collision-checked input list.
/// Directories are walked recursively in sorted order; non-regular directory
/// entries are skipped with a warning. Archive-path collisions are an error:
/// two members with one name would silently shadow each other at restore.
fn collect_inputs(paths: &[PathBuf], warnings: &mut Vec<String>) -> Result<Vec<PutInput>, String> {
    let mut inputs = Vec::new();
    let mut stripped_absolute = false;
    for path in paths {
        let metadata =
            std::fs::metadata(path).map_err(|error| format!("stat {}: {error}", path.display()))?;
        if metadata.is_file() {
            inputs.push(PutInput {
                fs_path: path.clone(),
                archive_path: archive_path_for(path, &mut stripped_absolute)?,
                size_bytes: metadata.len(),
            });
        } else if metadata.is_dir() {
            walk_dir(path, &mut inputs, warnings, &mut stripped_absolute)?;
        } else {
            warnings.push(format!("skipping {}: not a regular file", path.display()));
        }
    }
    if stripped_absolute {
        warnings.push("removing leading '/' from archive member paths".to_string());
    }
    if inputs.is_empty() {
        return Err("no regular files to archive".to_string());
    }

    let mut seen: HashSet<&str> = HashSet::new();
    let mut collisions: Vec<&str> = Vec::new();
    for input in &inputs {
        if !seen.insert(&input.archive_path) {
            collisions.push(&input.archive_path);
        }
    }
    if !collisions.is_empty() {
        return Err(format!(
            "archive path collision{}: {} — members would shadow each other; \
             archive the colliding inputs in separate invocations",
            if collisions.len() == 1 { "" } else { "s" },
            collisions.join(", "),
        ));
    }
    Ok(inputs)
}

fn walk_dir(
    dir: &Path,
    inputs: &mut Vec<PutInput>,
    warnings: &mut Vec<String>,
    stripped_absolute: &mut bool,
) -> Result<(), String> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|error| format!("read directory {}: {error}", dir.display()))?
        .collect::<Result<_, _>>()
        .map_err(|error| format!("read directory {}: {error}", dir.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        // symlink_metadata inside the walk: following links out of the tree
        // would silently archive content the operator did not name.
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("stat {}: {error}", path.display()))?;
        if metadata.is_dir() {
            walk_dir(&path, inputs, warnings, stripped_absolute)?;
        } else if metadata.is_file() {
            inputs.push(PutInput {
                archive_path: archive_path_for(&path, stripped_absolute)?,
                fs_path: path,
                size_bytes: metadata.len(),
            });
        } else {
            warnings.push(format!("skipping {}: not a regular file", path.display()));
        }
    }
    Ok(())
}

/// Map a filesystem path to its archive member path, tar-style: leading `/`
/// and `.` components drop, `..` is refused outright — a member path that
/// escapes its root is exactly the restore-time surprise this tool exists to
/// prevent.
pub(crate) fn archive_path_for(
    path: &Path,
    stripped_absolute: &mut bool,
) -> Result<String, String> {
    use std::path::Component;
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => match part.to_str() {
                Some(part) => parts.push(part),
                None => {
                    return Err(format!(
                        "{}: non-UTF-8 path components are not supported",
                        path.display()
                    ));
                }
            },
            Component::RootDir | Component::Prefix(_) => *stripped_absolute = true,
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(format!(
                    "{}: '..' components are not allowed in archive paths; \
                     resolve the path first",
                    path.display()
                ));
            }
        }
    }
    if parts.is_empty() {
        return Err(format!("{}: empty archive path", path.display()));
    }
    Ok(parts.join("/"))
}

/// Parse repeated `--meta key=value` pairs. `path` is reserved: the daemon
/// reads it as the member's archive path, which put owns.
fn parse_meta(specs: &[String]) -> Result<HashMap<String, String>, String> {
    let mut meta = HashMap::new();
    for spec in specs {
        let (key, value) = spec
            .split_once('=')
            .ok_or_else(|| format!("--meta {spec:?} is not key=value"))?;
        let key = key.trim();
        if key.is_empty() {
            return Err(format!("--meta {spec:?} has an empty key"));
        }
        if key == "path" {
            return Err("--meta key \"path\" is reserved for the archive member path".to_string());
        }
        if meta.insert(key.to_string(), value.to_string()).is_some() {
            return Err(format!("--meta key {key:?} given twice"));
        }
    }
    Ok(meta)
}

// ---------------------------------------------------------------------------
// Target resolution

async fn resolve_target(args: &PutArgs, channel: Channel) -> Result<PutTarget, DaemonClientError> {
    if let Some(tape) = &args.tape {
        let tape_uuid = Uuid::parse_str(tape.trim())
            .map_err(|error| DaemonClientError::client(format!("--tape {tape:?}: {error}")))?;
        // clap enforces `requires = "pool"`; the expect documents the invariant.
        let required_pool_id = args
            .pool
            .as_deref()
            .expect("clap requires --pool with --tape")
            .trim()
            .to_string();
        return Ok(PutTarget::Tape {
            tape_uuid: tape_uuid.as_bytes().to_vec(),
            required_pool_id,
        });
    }
    if let Some(element) = args.drive {
        let library_uuid = resolve_library_uuid(channel, args.library.as_deref(), true).await?;
        return Ok(PutTarget::Drive {
            library_uuid,
            element,
        });
    }
    let library_uuid =
        resolve_library_uuid(channel.clone(), args.library.as_deref(), false).await?;
    let pool_id = match &args.pool {
        Some(pool) => pool.trim().to_string(),
        None => {
            let pools = pb::catalog_client::CatalogClient::new(channel)
                .list_tape_pools(pb::ListTapePoolsRequest {
                    page_token: None,
                    page_size: 0,
                })
                .await
                .map_err(status_error)?
                .into_inner()
                .pools;
            choose_default_pool(&pools).map_err(DaemonClientError::client)?
        }
    };
    Ok(PutTarget::Pool {
        pool_id,
        library_uuid,
    })
}

/// The zero-config case: one configured pool needs no `--pool`. More than one
/// means the choice is real policy, so it must be explicit.
fn choose_default_pool(pools: &[pb::TapePool]) -> Result<String, String> {
    match pools {
        [] => Err(
            "no tape pools are configured; declare one in the daemon config \
             or pass --tape/--drive"
                .to_string(),
        ),
        [only] => Ok(only.pool_id.clone()),
        many => Err(format!(
            "{} pools are configured ({}); pass --pool to choose one",
            many.len(),
            many.iter()
                .map(|pool| pool.pool_id.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        )),
    }
}

async fn resolve_library_uuid(
    channel: Channel,
    serial: Option<&str>,
    required: bool,
) -> Result<Vec<u8>, DaemonClientError> {
    let serial = serial.map(str::trim).filter(|serial| !serial.is_empty());
    if serial.is_none() && !required {
        return Ok(Vec::new());
    }
    let libraries = pb::library_service_client::LibraryServiceClient::new(channel)
        .list_libraries(())
        .await
        .map_err(status_error)?
        .into_inner()
        .libraries;
    match serial {
        Some(serial) => libraries
            .into_iter()
            .find(|library| library.library_serial == serial)
            .map(|library| library.library_uuid)
            .ok_or_else(|| {
                DaemonClientError::client(format!("library serial {serial:?} not found by daemon"))
            }),
        None => match libraries.as_slice() {
            [only] => Ok(only.library_uuid.clone()),
            _ => Err(DaemonClientError::client(format!(
                "--drive needs --library when {} libraries are attached",
                libraries.len()
            ))),
        },
    }
}

// ---------------------------------------------------------------------------
// Session lifecycle

async fn open_write_session(
    channel: Channel,
    client: &mut pb::write_session_service_client::WriteSessionServiceClient<Channel>,
    target: &PutTarget,
    no_wait: bool,
    err: &mut dyn Write,
) -> Result<Vec<u8>, DaemonClientError> {
    let request = || pb::OpenWriteSessionRequest {
        target: Some(target.to_proto()),
        body_format: "rem-object-v1".to_string(),
        idempotency_key: None,
        recover_session_id: Vec::new(),
    };
    match client.open_write_session(request()).await {
        Ok(response) => Ok(response.into_inner().session_id),
        Err(status) if !no_wait => {
            wait_before_open_retry(channel, &status, err).await?;
            Ok(client
                .open_write_session(request())
                .await
                .map_err(status_error)?
                .into_inner()
                .session_id)
        }
        Err(status) => Err(status_error(status)),
    }
}

/// Mirror of the remfield-io open-retry contract: a FailedPrecondition that
/// names a media-readiness operation means a tape load is in flight — watch it
/// to completion, then retry once. A busy drive bay gets one short retry.
/// Anything else is a real error.
pub(crate) async fn wait_before_open_retry(
    channel: Channel,
    status: &tonic::Status,
    err: &mut dyn Write,
) -> Result<(), DaemonClientError> {
    if status.code() != tonic::Code::FailedPrecondition {
        return Err(status_error(status.clone()));
    }
    if let Some(operation_id) = media_readiness_operation_id(status.message()) {
        let _ = writeln!(
            err,
            "open fenced by media readiness operation {operation_id}; waiting \
             up to {DEFAULT_READY_TIMEOUT_SECONDS}s (pass --no-wait to fail fast)",
        );
        return wait_for_media_readiness_operation(channel, operation_id, err).await;
    }
    if drive_bay_busy(status.message()) {
        let _ = writeln!(
            err,
            "open found a busy drive bay; retrying once after {:.0}s",
            DRIVE_BUSY_RETRY_DELAY.as_secs_f64()
        );
        tokio::time::sleep(DRIVE_BUSY_RETRY_DELAY).await;
        return Ok(());
    }
    Err(status_error(status.clone()))
}

fn media_readiness_operation_id(message: &str) -> Option<Uuid> {
    if !message.contains("media-readiness") {
        return None;
    }
    message.split_ascii_whitespace().find_map(|token| {
        let value = token.strip_prefix("operation=")?;
        Uuid::parse_str(value.trim_matches(|ch: char| !ch.is_ascii_hexdigit() && ch != '-')).ok()
    })
}

fn drive_bay_busy(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    (lower.contains("drive bay") && lower.contains("is busy"))
        || lower.contains("drive-session owner is busy")
}

async fn wait_for_media_readiness_operation(
    channel: Channel,
    operation_id: Uuid,
    err: &mut dyn Write,
) -> Result<(), DaemonClientError> {
    let mut library = pb::library_service_client::LibraryServiceClient::new(channel.clone());
    let operation = library
        .resume_media_readiness(pb::ResumeMediaReadinessRequest {
            operation_id: operation_id.as_bytes().to_vec(),
            timeout_seconds: DEFAULT_READY_TIMEOUT_SECONDS,
            poll_interval_seconds: DEFAULT_READY_POLL_SECONDS,
        })
        .await
        .map_err(status_error)?
        .into_inner();
    let mut stream = pb::daemon_client::DaemonClient::new(channel)
        .watch_operation(pb::GetOperationRequest {
            operation_id: operation.operation_id,
        })
        .await
        .map_err(status_error)?
        .into_inner();
    while let Some(status) = stream.message().await.map_err(status_error)? {
        let state =
            pb::OperationState::try_from(status.state).unwrap_or(pb::OperationState::Unspecified);
        let readiness = status
            .progress
            .get("state")
            .map(String::as_str)
            .unwrap_or("unknown");
        let _ = writeln!(err, "media readiness operation {operation_id}: {readiness}");
        match state {
            pb::OperationState::Succeeded => return Ok(()),
            pb::OperationState::Failed
            | pb::OperationState::Cancelled
            | pb::OperationState::CompletionUnknown => {
                let summary = if status.error_summary.is_empty() {
                    format!("finished {state:?}")
                } else {
                    status.error_summary
                };
                return Err(DaemonClientError::client(format!(
                    "media readiness operation {operation_id} did not reach READY: {summary}"
                )));
            }
            _ => {}
        }
    }
    Err(DaemonClientError::client(format!(
        "media readiness operation {operation_id} watch ended before READY"
    )))
}

/// Best-effort abort. Returns whether the daemon acknowledged it, so the
/// caller's error message can state the true session disposition.
async fn abort_write_session(
    client: &mut pb::write_session_service_client::WriteSessionServiceClient<Channel>,
    session_id: &[u8],
    err: &mut dyn Write,
) -> bool {
    match client
        .abort_write_session(pb::AbortWriteSessionRequest {
            session_id: session_id.to_vec(),
            idempotency_key: None,
            reason: "rem put: append failed, aborting the batch".to_string(),
        })
        .await
    {
        Ok(_) => true,
        Err(status) => {
            let _ = writeln!(
                err,
                "warning: abort of session {} failed ({})",
                format_uuid(session_id),
                status.message(),
            );
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Append

async fn append_one(
    client: &mut pb::write_session_service_client::WriteSessionServiceClient<Channel>,
    session_id: &[u8],
    input: &PutInput,
    caller_object_id: &str,
    extra_meta: &HashMap<String, String>,
    chunk_bytes: usize,
) -> Result<pb::ObjectRecord, DaemonClientError> {
    // Pre-hash so the daemon can admit the append in overlap mode and verify
    // the spooled bytes against a digest it received before the first chunk.
    // One extra local read per file is the documented price.
    let content_sha256 = hash_file(&input.fs_path).await?;

    let (tx, rx) = tokio::sync::mpsc::channel::<pb::AppendObjectMessage>(8);
    let session_id_owned = session_id.to_vec();
    let fs_path = input.fs_path.clone();
    let archive_path = input.archive_path.clone();
    let caller_object_id = caller_object_id.to_string();
    let declared_size_bytes = input.size_bytes;
    let mut caller_metadata = extra_meta.clone();
    caller_metadata.insert("path".to_string(), archive_path.clone());

    let sender = tokio::spawn(async move {
        let digest = pb::Digest {
            algorithm: "sha256".to_string(),
            value: content_sha256.to_vec(),
        };
        tx.send(pb::AppendObjectMessage {
            payload: Some(pb::append_object_message::Payload::Start(
                pb::AppendObjectStart {
                    session_id: session_id_owned.clone(),
                    caller_object_id,
                    caller_metadata,
                    declared_size_bytes,
                    body_format_manifest: Vec::new(),
                    expected_content_sha256: content_sha256.to_vec(),
                    expected_content_digest: Some(digest.clone()),
                    source_replay_capability: pb::SourceReplayCapability::ReplayFromStart as i32,
                },
            )),
        })
        .await
        .map_err(|_| "append stream closed before Start".to_string())?;

        let mut file = tokio::fs::File::open(&fs_path)
            .await
            .map_err(|error| format!("open {}: {error}", fs_path.display()))?;
        let mut buffer = vec![0_u8; chunk_bytes];
        loop {
            let read = file
                .read(&mut buffer)
                .await
                .map_err(|error| format!("read {}: {error}", fs_path.display()))?;
            if read == 0 {
                break;
            }
            tx.send(pb::AppendObjectMessage {
                payload: Some(pb::append_object_message::Payload::Chunk(
                    pb::AppendObjectChunk {
                        session_id: session_id_owned.clone(),
                        data: buffer[..read].to_vec(),
                    },
                )),
            })
            .await
            .map_err(|_| "append stream closed mid-transfer".to_string())?;
        }

        tx.send(pb::AppendObjectMessage {
            payload: Some(pb::append_object_message::Payload::Finish(
                pb::AppendObjectFinish {
                    session_id: session_id_owned,
                    expected_content_sha256: content_sha256.to_vec(),
                    expected_content_digest: Some(digest),
                },
            )),
        })
        .await
        .map_err(|_| "append stream closed before Finish".to_string())?;
        Ok::<(), String>(())
    });

    let append = client.append_object(ReceiverStream::new(rx)).await;
    let sender_result = sender.await.map_err(|error| {
        DaemonClientError::client(format!("append sender task failed: {error}"))
    })?;
    // When the server rejects the stream early, the sender sees a closed
    // channel; the server's status is the real error, so prefer it.
    match (append, sender_result) {
        (Ok(response), _) => Ok(response.into_inner()),
        (Err(status), Ok(())) => Err(status_error(status)),
        (Err(status), Err(sender_error)) if sender_error.contains("closed") => {
            Err(status_error(status))
        }
        (Err(_), Err(sender_error)) => Err(DaemonClientError::client(sender_error)),
    }
}

async fn hash_file(path: &Path) -> Result<[u8; 32], DaemonClientError> {
    let mut file = tokio::fs::File::open(path).await.map_err(|error| {
        DaemonClientError::client(format!("open {} for hashing: {error}", path.display()))
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; DEFAULT_PUT_CHUNK_BYTES];
    loop {
        let read = file.read(&mut buffer).await.map_err(|error| {
            DaemonClientError::client(format!("hash {}: {error}", path.display()))
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

// ---------------------------------------------------------------------------
// Receipt

/// Best-effort voltag lookup for the receipt. A failure here never fails the
/// put: the write is already committed, and the UUID still identifies the
/// tape.
async fn resolve_voltags(
    catalog: &mut pb::catalog_client::CatalogClient<Channel>,
    receipts: &[(PutInput, pb::ObjectRecord)],
) -> HashMap<Vec<u8>, String> {
    let mut voltags = HashMap::new();
    for (_, object) in receipts {
        for copy in &object.copies {
            if voltags.contains_key(&copy.tape_uuid) {
                continue;
            }
            if let Ok(response) = catalog
                .get_tape(pb::GetTapeRequest {
                    tape_uuid: copy.tape_uuid.clone(),
                })
                .await
            {
                let tape = response.into_inner();
                if let Some(voltag) = tape.voltag {
                    voltags.insert(copy.tape_uuid.clone(), voltag);
                }
            }
        }
    }
    voltags
}

fn print_receipt(
    args: &PutArgs,
    target: &PutTarget,
    session: &pb::WriteSession,
    receipts: &[(PutInput, pb::ObjectRecord)],
    voltags: &HashMap<Vec<u8>, String>,
    out: &mut dyn Write,
) -> Result<(), String> {
    let tape_label = |tape_uuid: &Vec<u8>| {
        voltags
            .get(tape_uuid)
            .cloned()
            .unwrap_or_else(|| format_uuid(tape_uuid))
    };
    if args.json {
        let objects: Vec<serde_json::Value> = receipts
            .iter()
            .map(|(input, object)| {
                serde_json::json!({
                    "archive_path": input.archive_path,
                    "caller_object_id": object.caller_object_id,
                    "object_id": format_uuid(&object.object_id),
                    "size_bytes": object.logical_size_bytes,
                    "content_sha256": hex(&object.content_sha256),
                    "body_format": object.body_format,
                    "copies": object.copies.iter().map(|copy| {
                        serde_json::json!({
                            "tape_uuid": format_uuid(&copy.tape_uuid),
                            // null when the label lookup failed; tape_uuid
                            // above always identifies the tape. Deliberate:
                            // an unknown label is not a UUID-shaped string.
                            "voltag": voltags.get(&copy.tape_uuid),
                            "tape_file_number": copy.tape_file_number,
                            // Together with tape_uuid/tape_file_number and the
                            // object identity above, this makes each copy a
                            // complete canonical locator: the same fields the
                            // catalog and the daemon read path key on.
                            "first_body_lba": copy.first_body_lba,
                            "pool_id": copy.pool_id,
                        })
                    }).collect::<Vec<_>>(),
                })
            })
            .collect();
        let receipt = serde_json::json!({
            "session_id": format_uuid(&session.session_id),
            "target": target.describe(),
            "objects_committed": session.objects_committed,
            "bytes_committed": session.bytes_committed,
            "objects": objects,
        });
        writeln!(out, "{receipt}").map_err(|error| error.to_string())
    } else {
        for (input, object) in receipts {
            for copy in &object.copies {
                writeln!(
                    out,
                    "{}  object {}  tape {} file {}{}",
                    input.archive_path,
                    format_uuid(&object.object_id),
                    tape_label(&copy.tape_uuid),
                    copy.tape_file_number,
                    if copy.pool_id.is_empty() {
                        String::new()
                    } else {
                        format!("  pool {}", copy.pool_id)
                    },
                )
                .map_err(|error| error.to_string())?;
            }
        }
        writeln!(
            out,
            "session {} closed: {} object{}, {} committed",
            format_uuid(&session.session_id),
            session.objects_committed,
            if session.objects_committed == 1 {
                ""
            } else {
                "s"
            },
            format_bytes(session.bytes_committed),
        )
        .map_err(|error| error.to_string())
    }
}

pub(crate) fn format_uuid(bytes: &[u8]) -> String {
    Uuid::from_slice(bytes)
        .map(|uuid| uuid.to_string())
        .unwrap_or_else(|_| hex(bytes))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("GiB", 1 << 30),
        ("MiB", 1 << 20),
        ("KiB", 1 << 10),
        ("B", 1),
    ];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            return if scale == 1 {
                format!("{bytes} B")
            } else {
                format!("{:.1} {unit}", bytes as f64 / scale as f64)
            };
        }
    }
    "0 B".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use tonic::{Request, Response, Status, Streaming};

    // -- pure helpers -------------------------------------------------------

    fn touch(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn collect_inputs_walks_directories_sorted_and_relative() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("photos");
        touch(&root.join("b/two.jpg"), b"two");
        touch(&root.join("a/one.jpg"), b"one");
        touch(&root.join("top.txt"), b"top");
        let mut warnings = Vec::new();
        let inputs = collect_inputs(std::slice::from_ref(&root), &mut warnings).unwrap();
        let suffixes: Vec<&str> = inputs
            .iter()
            .map(|input| input.archive_path.as_str())
            .collect();
        // Absolute inputs keep their (stripped) prefix; ordering and relative
        // structure are what this test pins.
        assert_eq!(suffixes.len(), 3);
        assert!(suffixes[0].ends_with("photos/a/one.jpg"), "{suffixes:?}");
        assert!(suffixes[1].ends_with("photos/b/two.jpg"), "{suffixes:?}");
        assert!(suffixes[2].ends_with("photos/top.txt"), "{suffixes:?}");
    }

    #[test]
    fn collect_inputs_rejects_archive_path_collisions() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("x/a.txt"), b"1");
        let mut warnings = Vec::new();
        let file = dir.path().join("x/a.txt");
        let error = collect_inputs(&[file.clone(), file], &mut warnings).unwrap_err();
        assert!(error.contains("collision"), "{error}");
    }

    #[test]
    fn collect_inputs_strips_absolute_prefix_with_warning() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data.bin");
        touch(&file, b"abc");
        let mut warnings = Vec::new();
        let inputs = collect_inputs(std::slice::from_ref(&file), &mut warnings).unwrap();
        assert!(!inputs[0].archive_path.starts_with('/'));
        assert!(warnings
            .iter()
            .any(|warning| warning.contains("leading '/'")));
    }

    #[test]
    fn archive_path_refuses_parent_components() {
        let mut stripped = false;
        let error = archive_path_for(Path::new("a/../b"), &mut stripped).unwrap_err();
        assert!(error.contains(".."), "{error}");
    }

    #[test]
    fn parse_meta_rejects_reserved_and_duplicate_keys() {
        assert!(parse_meta(&["path=x".to_string()])
            .unwrap_err()
            .contains("reserved"));
        assert!(parse_meta(&["k=1".to_string(), "k=2".to_string()])
            .unwrap_err()
            .contains("twice"));
        assert!(parse_meta(&["novalue".to_string()])
            .unwrap_err()
            .contains("key=value"));
        let meta = parse_meta(&["k=v=w".to_string()]).unwrap();
        assert_eq!(meta["k"], "v=w");
    }

    #[test]
    fn choose_default_pool_needs_exactly_one() {
        let pool = |id: &str| pb::TapePool {
            pool_id: id.to_string(),
            display_name: String::new(),
            copy_class: String::new(),
            content_class: String::new(),
        };
        assert!(choose_default_pool(&[]).is_err());
        assert_eq!(choose_default_pool(&[pool("solo")]).unwrap(), "solo");
        let error = choose_default_pool(&[pool("a"), pool("b")]).unwrap_err();
        assert!(error.contains("a, b"), "{error}");
    }

    // -- fake daemon --------------------------------------------------------

    #[derive(Default)]
    struct FakeState {
        records: Mutex<Vec<pb::ObjectRecord>>,
        aborted: AtomicBool,
        fail_appends_containing: Option<String>,
        fail_abort: bool,
        /// Checkpoint omits objects whose caller id contains this marker,
        /// simulating an earlier auto-checkpoint having committed them.
        checkpoint_omits_containing: Option<String>,
        /// When set, open must arrive as a TapeTarget with this uuid+guard
        /// (instead of the default pool-target assertion).
        expect_tape_target: Option<(Vec<u8>, String)>,
    }

    struct FakeWriteSessions(Arc<FakeState>);

    const FAKE_SESSION: [u8; 16] = [9; 16];
    const FAKE_TAPE: [u8; 16] = [7; 16];

    #[tonic::async_trait]
    impl pb::write_session_service_server::WriteSessionService for FakeWriteSessions {
        async fn open_write_session(
            &self,
            request: Request<pb::OpenWriteSessionRequest>,
        ) -> Result<Response<pb::WriteSession>, Status> {
            let request = request.into_inner();
            assert_eq!(request.body_format, "rem-object-v1");
            match (&self.0.expect_tape_target, request.target) {
                (None, Some(pb::open_write_session_request::Target::PoolTarget(target))) => {
                    assert_eq!(target.pool_id, "solo");
                    assert!(target.mount_if_needed);
                }
                (
                    Some((tape_uuid, guard)),
                    Some(pb::open_write_session_request::Target::TapeTarget(target)),
                ) => {
                    assert_eq!(&target.tape_uuid, tape_uuid);
                    assert_eq!(&target.required_pool_id, guard);
                    assert!(target.mount_if_needed);
                    assert!(!target.allow_unpooled);
                }
                (expected, other) => panic!("unexpected target {other:?} (expected {expected:?})"),
            }
            Ok(Response::new(pb::WriteSession {
                session_id: FAKE_SESSION.to_vec(),
                ..Default::default()
            }))
        }

        async fn append_object(
            &self,
            request: Request<Streaming<pb::AppendObjectMessage>>,
        ) -> Result<Response<pb::ObjectRecord>, Status> {
            let mut stream = request.into_inner();
            let mut caller_object_id = String::new();
            let mut caller_metadata = HashMap::new();
            let mut hasher = Sha256::new();
            let mut size = 0_u64;
            let mut expected = Vec::new();
            while let Some(message) = stream.message().await? {
                match message.payload.unwrap() {
                    pb::append_object_message::Payload::Start(start) => {
                        assert_eq!(start.session_id, FAKE_SESSION.to_vec());
                        caller_object_id = start.caller_object_id;
                        caller_metadata = start.caller_metadata;
                        expected = start.expected_content_sha256;
                    }
                    pb::append_object_message::Payload::Chunk(chunk) => {
                        size += chunk.data.len() as u64;
                        hasher.update(&chunk.data);
                    }
                    pb::append_object_message::Payload::Finish(_) => {}
                }
            }
            if let Some(marker) = &self.0.fail_appends_containing {
                if caller_object_id.contains(marker.as_str()) {
                    return Err(Status::data_loss("injected append failure"));
                }
            }
            let digest: [u8; 32] = hasher.finalize().into();
            assert_eq!(expected, digest.to_vec(), "client-declared digest mismatch");
            let record = pb::ObjectRecord {
                object_id: Uuid::new_v4().as_bytes().to_vec(),
                caller_object_id,
                content_sha256: digest.to_vec(),
                logical_size_bytes: size,
                caller_metadata,
                body_format: "rem-object-v1".to_string(),
                ..Default::default()
            };
            self.0.records.lock().unwrap().push(record.clone());
            Ok(Response::new(record))
        }

        async fn checkpoint_session(
            &self,
            _request: Request<pb::CheckpointSessionRequest>,
        ) -> Result<Response<pb::CheckpointSessionResponse>, Status> {
            let records = self.0.records.lock().unwrap();
            let committed_objects: Vec<pb::ObjectRecord> = records
                .iter()
                .enumerate()
                .filter(|(_, record)| {
                    self.0
                        .checkpoint_omits_containing
                        .as_ref()
                        .is_none_or(|marker| !record.caller_object_id.contains(marker.as_str()))
                })
                .map(|(index, record)| pb::ObjectRecord {
                    copies: vec![pb::ObjectCopy {
                        tape_uuid: FAKE_TAPE.to_vec(),
                        tape_file_number: 100 + index as u64,
                        first_body_lba: 7 * index as u64,
                        pool_id: "solo".to_string(),
                        ..Default::default()
                    }],
                    ..record.clone()
                })
                .collect();
            let committed_copies = committed_objects
                .iter()
                .flat_map(|object| object.copies.iter().cloned())
                .collect();
            Ok(Response::new(pb::CheckpointSessionResponse {
                session: None,
                committed_objects,
                committed_copies,
            }))
        }

        async fn close_write_session(
            &self,
            _request: Request<pb::CloseWriteSessionRequest>,
        ) -> Result<Response<pb::WriteSession>, Status> {
            let records = self.0.records.lock().unwrap();
            Ok(Response::new(pb::WriteSession {
                session_id: FAKE_SESSION.to_vec(),
                objects_committed: records.len() as u64,
                bytes_committed: records.iter().map(|r| r.logical_size_bytes).sum(),
                ..Default::default()
            }))
        }

        async fn abort_write_session(
            &self,
            _request: Request<pb::AbortWriteSessionRequest>,
        ) -> Result<Response<pb::WriteSession>, Status> {
            if self.0.fail_abort {
                return Err(Status::unavailable("injected abort failure"));
            }
            self.0.aborted.store(true, Ordering::SeqCst);
            Ok(Response::new(pb::WriteSession {
                session_id: FAKE_SESSION.to_vec(),
                ..Default::default()
            }))
        }

        async fn get_write_session(
            &self,
            _request: Request<pb::GetWriteSessionRequest>,
        ) -> Result<Response<pb::WriteSession>, Status> {
            Err(Status::unimplemented("not needed by these tests"))
        }
    }

    /// Serve the fake over a unix socket on a background runtime; return the
    /// endpoint string and a guard that shuts the runtime down on drop.
    fn serve_fake(state: Arc<FakeState>) -> (String, tokio::runtime::Runtime, tempfile::TempDir) {
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
                .add_service(
                    pb::write_session_service_server::WriteSessionServiceServer::new(
                        FakeWriteSessions(state),
                    ),
                )
                .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(listener)),
        );
        (format!("unix:{}", socket.display()), runtime, dir)
    }

    fn put_args(paths: Vec<PathBuf>, endpoint: String) -> PutArgs {
        PutArgs {
            paths,
            pool: Some("solo".to_string()),
            tape: None,
            drive: None,
            library: None,
            id: None,
            meta: Vec::new(),
            chunk_bytes: 8, // several chunks even for tiny test files
            no_wait: true,
            endpoint,
            json: true,
        }
    }

    fn run_put_blocking(args: &PutArgs) -> (Result<(), DaemonClientError>, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = daemon_runtime()
            .and_then(|runtime| runtime.block_on(async { put(args, &mut out, &mut err).await }));
        (
            result,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn put_streams_files_and_reports_committed_copies() {
        let state = Arc::new(FakeState::default());
        let (endpoint, _runtime, _dir) = serve_fake(state.clone());

        let data_dir = tempfile::tempdir().unwrap();
        touch(&data_dir.path().join("in/a.txt"), b"alpha alpha alpha");
        touch(&data_dir.path().join("in/b.txt"), b"beta");
        let args = put_args(vec![data_dir.path().join("in")], endpoint);

        let (result, out, err) = run_put_blocking(&args);
        assert!(result.is_ok(), "{result:?}\nstderr: {err}");

        let receipt: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        let objects = receipt["objects"].as_array().unwrap();
        assert_eq!(objects.len(), 2);
        assert_eq!(receipt["objects_committed"], 2);
        for (index, object) in objects.iter().enumerate() {
            assert!(object["archive_path"].as_str().unwrap().ends_with(".txt"));
            assert_eq!(object["body_format"], "rem-object-v1");
            let copy = &object["copies"][0];
            assert_eq!(copy["pool_id"], "solo");
            assert_eq!(copy["tape_file_number"], 100 + index as u64);
            // The receipt copy must be a complete canonical locator.
            assert_eq!(copy["first_body_lba"], 7 * index as u64);
        }
        // The fake hashed what it received and asserted it matched the
        // client's declared digest, so a passing run proves byte fidelity.
        assert!(!state.aborted.load(Ordering::SeqCst));
    }

    #[test]
    fn tape_flag_requires_the_pool_guard() {
        #[derive(clap::Parser, Debug)]
        struct Harness {
            #[command(flatten)]
            args: PutArgs,
        }
        let uuid = Uuid::from_bytes([3; 16]).to_string();
        // Without --pool the parse must fail: the guard is mandatory.
        let error =
            <Harness as clap::Parser>::try_parse_from(["put", "--tape", &uuid, "some-file"])
                .expect_err("--tape without --pool must not parse");
        assert!(error.to_string().contains("--pool"), "{error}");
        // With the guard it parses.
        let parsed = <Harness as clap::Parser>::try_parse_from([
            "put",
            "--tape",
            &uuid,
            "--pool",
            "camera",
            "some-file",
        ])
        .expect("--tape with --pool parses");
        assert_eq!(parsed.args.pool.as_deref(), Some("camera"));
    }

    #[test]
    fn put_pins_a_tape_with_the_pool_guard() {
        let tape_uuid = Uuid::from_bytes([3; 16]);
        let state = Arc::new(FakeState {
            expect_tape_target: Some((tape_uuid.as_bytes().to_vec(), "solo".to_string())),
            ..FakeState::default()
        });
        let (endpoint, _runtime, _dir) = serve_fake(state.clone());

        let data_dir = tempfile::tempdir().unwrap();
        touch(&data_dir.path().join("in/a.txt"), b"pinned payload");
        let mut args = put_args(vec![data_dir.path().join("in")], endpoint);
        args.tape = Some(tape_uuid.to_string());

        let (result, out, err) = run_put_blocking(&args);
        assert!(result.is_ok(), "{result:?}\nstderr: {err}");
        let receipt: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(receipt["objects"].as_array().unwrap().len(), 1);
        assert!(
            receipt["target"].as_str().unwrap().contains("pool solo"),
            "{}",
            receipt["target"]
        );
    }

    #[test]
    fn put_reports_true_disposition_when_abort_also_fails() {
        let state = Arc::new(FakeState {
            fail_appends_containing: Some("poison".to_string()),
            fail_abort: true,
            ..FakeState::default()
        });
        let (endpoint, _runtime, _dir) = serve_fake(state.clone());

        let data_dir = tempfile::tempdir().unwrap();
        touch(&data_dir.path().join("in/poison.txt"), b"bad");
        let args = put_args(vec![data_dir.path().join("in")], endpoint);

        let (result, _out, _err) = run_put_blocking(&args);
        let error = result.unwrap_err();
        // The message must NOT claim the appends were discarded: the abort
        // failed, so the session is open and its appends are recoverable.
        assert!(error.message.contains("left open"), "{}", error.message);
        assert!(!error.message.contains("discarded"), "{}", error.message);
        assert!(!state.aborted.load(Ordering::SeqCst));
    }

    #[test]
    fn put_keeps_confirmed_receipts_when_one_object_cannot_be_verified() {
        // "early" objects are absent from the checkpoint response, as after
        // an auto-checkpoint. The catalog service is not registered on the
        // fake server, so the fallback lookup fails — the confirmed object's
        // receipt must still print, and the error must say "unknown", not
        // claim the object uncommitted.
        let state = Arc::new(FakeState {
            checkpoint_omits_containing: Some("early".to_string()),
            ..FakeState::default()
        });
        let (endpoint, _runtime, _dir) = serve_fake(state.clone());

        let data_dir = tempfile::tempdir().unwrap();
        touch(&data_dir.path().join("in/early.txt"), b"first");
        touch(&data_dir.path().join("in/late.txt"), b"second");
        let args = put_args(vec![data_dir.path().join("in")], endpoint);

        let (result, out, _err) = run_put_blocking(&args);
        let error = result.unwrap_err();
        assert!(
            error.message.contains("1 of 2 objects confirmed"),
            "{}",
            error.message
        );
        assert!(error.message.contains("early.txt"), "{}", error.message);
        assert!(
            error.message.contains("commit state unknown"),
            "{}",
            error.message
        );
        let receipt: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        let objects = receipt["objects"].as_array().unwrap();
        assert_eq!(objects.len(), 1, "confirmed receipt must survive");
        assert!(objects[0]["archive_path"]
            .as_str()
            .unwrap()
            .ends_with("late.txt"));
    }

    #[test]
    fn put_aborts_the_session_when_an_append_fails() {
        let state = Arc::new(FakeState {
            fail_appends_containing: Some("poison".to_string()),
            ..FakeState::default()
        });
        let (endpoint, _runtime, _dir) = serve_fake(state.clone());

        let data_dir = tempfile::tempdir().unwrap();
        touch(&data_dir.path().join("in/a.txt"), b"fine");
        touch(&data_dir.path().join("in/poison.txt"), b"bad");
        let args = put_args(vec![data_dir.path().join("in")], endpoint);

        let (result, _out, _err) = run_put_blocking(&args);
        let error = result.unwrap_err();
        assert!(error.message.contains("poison"), "{}", error.message);
        assert!(error.message.contains("aborted"), "{}", error.message);
        assert!(state.aborted.load(Ordering::SeqCst), "abort was not sent");
    }
}
