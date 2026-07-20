//! Minimal Remanence field-test I/O client.
//!
//! `remfield-io` lives under `fieldtest/` because it is bash-facing test
//! tooling, not an operator product surface. It drives the Layer 5 daemon
//! write/read session RPCs directly on systems where the field kit cannot rely
//! on Python gRPC bindings.

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{error::ErrorKind, Parser, Subcommand};
use remanence_api::pb;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use uuid::Uuid;

const DEFAULT_ENDPOINT: &str = "unix:/var/lib/rem/rem.sock";
const DEFAULT_CHUNK_BYTES: usize = 1_048_576;
const DEFAULT_CHUNK_BYTES_U32: u32 = 1_048_576;
const WRITE_MANY_APPEND_FAILED_EXIT: u8 = 12;
const DEFAULT_READY_TIMEOUT_SECONDS: u32 = 9_000;
const DEFAULT_READY_POLL_SECONDS: u32 = 30;
const DRIVE_BUSY_RETRY_DELAY: Duration = Duration::from_secs(1);

type AppResult<T> = Result<T, AppError>;

#[derive(Debug)]
struct AppError {
    message: String,
    exit_code: u8,
}

impl AppError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: 1,
        }
    }

    fn with_exit_code(message: impl Into<String>, exit_code: u8) -> Self {
        Self {
            message: message.into(),
            exit_code,
        }
    }

    fn exit_code(&self) -> u8 {
        self.exit_code
    }

    fn is_append_stream_closed(&self) -> bool {
        self.message.starts_with("append stream closed ")
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(error: std::io::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl From<tonic::Status> for AppError {
    fn from(status: tonic::Status) -> Self {
        Self::new(status.to_string())
    }
}

impl From<tonic::transport::Error> for AppError {
    fn from(error: tonic::transport::Error) -> Self {
        Self::new(error.to_string())
    }
}

#[derive(Parser, Debug)]
#[command(name = "remfield-io")]
#[command(about = "Field-test daemon write/read helper")]
struct Cli {
    #[arg(long, global = true, default_value = DEFAULT_ENDPOINT)]
    endpoint: String,

    /// Return immediately on media-readiness fences and busy drive bays.
    #[arg(long, global = true)]
    no_wait: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    Write(WriteArgs),
    WriteMany(WriteManyArgs),
    Read(ReadArgs),
    List(ListArgs),
}

#[derive(Parser, Debug)]
struct WriteArgs {
    #[arg(long)]
    file: PathBuf,

    #[arg(long)]
    pool: String,

    #[arg(long)]
    library: Option<String>,

    #[arg(long, default_value_t = DEFAULT_CHUNK_BYTES)]
    chunk_bytes: usize,

    /// Omit overlap admission proofs and force the legacy full-spool path.
    #[arg(long)]
    serial: bool,
}

#[derive(Parser, Debug)]
struct WriteManyArgs {
    #[arg(long)]
    pool: String,

    #[arg(long)]
    library: Option<String>,

    #[arg(long)]
    count: u64,

    #[arg(long)]
    size_mib: u64,

    #[arg(long)]
    caller_object_id_prefix: String,

    #[arg(long, default_value_t = DEFAULT_CHUNK_BYTES)]
    chunk_bytes: usize,
}

#[derive(Parser, Debug)]
struct ReadArgs {
    #[arg(long)]
    object: String,

    #[arg(long)]
    out: PathBuf,

    #[arg(long, default_value_t = 0)]
    offset: u64,

    #[arg(long)]
    length: Option<u64>,

    #[arg(long, default_value_t = DEFAULT_CHUNK_BYTES_U32)]
    chunk_bytes: u32,
}

#[derive(Parser, Debug)]
struct ListArgs {
    #[arg(long)]
    pool: String,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            print!("{error}");
            return ExitCode::from(error.exit_code() as u8);
        }
        Err(error) if error.use_stderr() => {
            print_json_error(error.to_string());
            return ExitCode::from(error.exit_code() as u8);
        }
        Err(error) => return ExitCode::from(error.exit_code() as u8),
    };

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let exit_code = error.exit_code();
            print_json_error(error.to_string());
            ExitCode::from(exit_code)
        }
    }
}

async fn run(cli: Cli) -> AppResult<()> {
    match cli.command {
        Command::Write(args) => write_command(&cli.endpoint, args, cli.no_wait).await,
        Command::WriteMany(args) => write_many_command(&cli.endpoint, args, cli.no_wait).await,
        Command::Read(args) => read_command(&cli.endpoint, args, cli.no_wait).await,
        Command::List(args) => list_command(&cli.endpoint, args).await,
    }
}

async fn connect_daemon(endpoint: &str) -> AppResult<Channel> {
    if let Some(path) = endpoint.strip_prefix("unix:") {
        return remanence_api::connect_unix(PathBuf::from(path))
            .await
            .map_err(|error| AppError::new(format!("connect daemon at {endpoint}: {error}")));
    }

    Channel::from_shared(endpoint.to_string())
        .map_err(|error| AppError::new(format!("invalid daemon endpoint {endpoint:?}: {error}")))?
        .connect()
        .await
        .map_err(|error| AppError::new(format!("connect daemon at {endpoint}: {error}")))
}

async fn wait_before_open_retry(channel: Channel, status: &tonic::Status) -> AppResult<()> {
    if status.code() != tonic::Code::FailedPrecondition {
        return Err(status.clone().into());
    }
    if let Some(operation_id) = media_readiness_operation_id(status.message()) {
        eprintln!(
            "remfield-io: open fenced by media readiness operation {operation_id}; waiting up to {}s",
            DEFAULT_READY_TIMEOUT_SECONDS
        );
        return wait_for_media_readiness_operation(channel, operation_id).await;
    }
    if drive_bay_busy(status.message()) {
        eprintln!(
            "remfield-io: open found a busy drive bay; retrying once after {:.0}s",
            DRIVE_BUSY_RETRY_DELAY.as_secs_f64()
        );
        tokio::time::sleep(DRIVE_BUSY_RETRY_DELAY).await;
        return Ok(());
    }
    Err(status.clone().into())
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

async fn wait_for_media_readiness_operation(channel: Channel, operation_id: Uuid) -> AppResult<()> {
    let mut library = pb::library_service_client::LibraryServiceClient::new(channel.clone());
    let operation = library
        .resume_media_readiness(pb::ResumeMediaReadinessRequest {
            operation_id: operation_id.as_bytes().to_vec(),
            timeout_seconds: DEFAULT_READY_TIMEOUT_SECONDS,
            poll_interval_seconds: DEFAULT_READY_POLL_SECONDS,
        })
        .await?
        .into_inner();
    let mut stream = pb::daemon_client::DaemonClient::new(channel)
        .watch_operation(pb::GetOperationRequest {
            operation_id: operation.operation_id,
        })
        .await?
        .into_inner();
    while let Some(status) = stream.message().await? {
        let state =
            pb::OperationState::try_from(status.state).unwrap_or(pb::OperationState::Unspecified);
        let readiness = status
            .progress
            .get("state")
            .map(String::as_str)
            .unwrap_or("unknown");
        let attempts = status
            .progress
            .get("attempts")
            .map(String::as_str)
            .unwrap_or("-");
        let elapsed = status
            .progress
            .get("elapsed_seconds")
            .map(String::as_str)
            .unwrap_or("-");
        eprintln!(
            "remfield-io: media readiness operation {operation_id} state={readiness} attempts={attempts} elapsed_s={elapsed}"
        );
        match state {
            pb::OperationState::Succeeded => {
                eprintln!(
                    "remfield-io: media readiness operation {operation_id} reached READY; retrying open once"
                );
                return Ok(());
            }
            pb::OperationState::Failed
            | pb::OperationState::Cancelled
            | pb::OperationState::CompletionUnknown => {
                let summary = if status.error_summary.is_empty() {
                    format!("finished {state:?}")
                } else {
                    status.error_summary
                };
                return Err(AppError::new(format!(
                    "media readiness operation {operation_id} did not reach READY: {summary}"
                )));
            }
            _ => {}
        }
    }
    Err(AppError::new(format!(
        "media readiness operation {operation_id} watch ended before READY"
    )))
}

async fn write_command(endpoint: &str, args: WriteArgs, no_wait: bool) -> AppResult<()> {
    if args.chunk_bytes == 0 {
        return Err(AppError::new("--chunk-bytes must be greater than zero"));
    }
    let metadata = tokio::fs::metadata(&args.file)
        .await
        .map_err(|error| AppError::new(format!("stat {}: {error}", args.file.display())))?;
    if !metadata.is_file() {
        return Err(AppError::new(format!(
            "--file {} is not a regular file",
            args.file.display()
        )));
    }

    // This field client has only a local file, so overlap mode pays a pre-read
    // before Start. A production caller should pass its already-known
    // immutable digest instead of adding another read on the Remanence host.
    let content_sha256 = if args.serial {
        None
    } else {
        Some(hash_file_before_start(&args.file).await?)
    };

    let channel = connect_daemon(endpoint).await?;
    let library_uuid = resolve_library_uuid(channel.clone(), args.library.as_deref()).await?;
    let mut write_client =
        pb::write_session_service_client::WriteSessionServiceClient::new(channel.clone());

    let started = Instant::now();
    let open_started = Instant::now();
    let session_id = open_write_session(
        channel,
        &mut write_client,
        &args.pool,
        library_uuid,
        no_wait,
    )
    .await?;
    let open_ms = duration_ms(open_started.elapsed());

    let append_input =
        AppendInput::file(&args.file, args.chunk_bytes, metadata.len(), content_sha256);
    let transfer_started = Instant::now();
    let append_result = append_object(&mut write_client, &session_id, append_input).await;
    let transfer_ms = duration_ms(transfer_started.elapsed());
    let record = match append_result {
        Ok(record) => record,
        Err(error) => {
            abort_write_session(&mut write_client, &session_id).await;
            return Err(error);
        }
    };

    let close_started = Instant::now();
    write_client
        .close_write_session(pb::CloseWriteSessionRequest {
            session_id: session_id.clone(),
            idempotency_key: None,
        })
        .await?;
    let close_ms = duration_ms(close_started.elapsed());

    let seconds = started.elapsed().as_secs_f64();
    let bytes = metadata.len();
    print_json_line(write_result_json(
        &record,
        &args.pool,
        bytes,
        seconds,
        PhaseTimings {
            open_ms: Some(open_ms),
            transfer_ms: Some(transfer_ms),
            close_ms: Some(close_ms),
        },
    ))?;
    Ok(())
}

async fn write_many_command(endpoint: &str, args: WriteManyArgs, no_wait: bool) -> AppResult<()> {
    if args.chunk_bytes == 0 {
        return Err(AppError::new("--chunk-bytes must be greater than zero"));
    }
    if args.count == 0 {
        return Err(AppError::new("--count must be greater than zero"));
    }
    if args.size_mib == 0 {
        return Err(AppError::new("--size-mib must be greater than zero"));
    }
    if args.caller_object_id_prefix.trim().is_empty() {
        return Err(AppError::new("--caller-object-id-prefix must not be empty"));
    }
    let bytes_per_object = args
        .size_mib
        .checked_mul(1024 * 1024)
        .ok_or_else(|| AppError::new("--size-mib overflows u64 bytes"))?;

    let channel = connect_daemon(endpoint).await?;
    let library_uuid = resolve_library_uuid(channel.clone(), args.library.as_deref()).await?;
    let mut write_client =
        pb::write_session_service_client::WriteSessionServiceClient::new(channel.clone());

    let open_started = Instant::now();
    let session_id = open_write_session(
        channel,
        &mut write_client,
        &args.pool,
        library_uuid,
        no_wait,
    )
    .await?;
    let open_ms = duration_ms(open_started.elapsed());

    let mut object_records = Vec::new();
    for idx in 0..args.count {
        let append_input = AppendInput::generated(
            idx,
            bytes_per_object,
            args.chunk_bytes,
            format!("{}-{idx}", args.caller_object_id_prefix.trim()),
        );
        let transfer_started = Instant::now();
        match append_object(&mut write_client, &session_id, append_input).await {
            Ok(record) => {
                let transfer_ms = duration_ms(transfer_started.elapsed());
                object_records.push(write_many_object_json(
                    &record,
                    &args.pool,
                    idx,
                    bytes_per_object,
                    PhaseTimings {
                        open_ms: (idx == 0).then_some(open_ms),
                        transfer_ms: Some(transfer_ms),
                        close_ms: None,
                    },
                ));
            }
            Err(error) => {
                let transfer_ms = duration_ms(transfer_started.elapsed());
                let close_started = Instant::now();
                let close_result = close_write_session(&mut write_client, &session_id).await;
                let close_ms = duration_ms(close_started.elapsed());
                if let Some(last_record) = object_records.last_mut() {
                    last_record["close_ms"] = json!(close_ms);
                }
                for record in object_records {
                    print_json_line(record)?;
                }
                print_json_line(write_many_error_json(
                    idx,
                    &args.pool,
                    bytes_per_object,
                    &format!("{}-{idx}", args.caller_object_id_prefix.trim()),
                    error.to_string(),
                    PhaseTimings {
                        open_ms: (idx == 0).then_some(open_ms),
                        transfer_ms: Some(transfer_ms),
                        close_ms: (idx == 0).then_some(close_ms),
                    },
                ))?;
                print_json_line(write_many_summary_json(
                    &args.pool,
                    args.count,
                    bytes_per_object,
                    idx,
                    Some(idx),
                    Some(close_ms),
                    close_result.as_ref().err().map(ToString::to_string),
                ))?;
                return Err(AppError::with_exit_code(
                    format!(
                        "write-many append {idx} failed after {idx} committed object(s): {error}"
                    ),
                    WRITE_MANY_APPEND_FAILED_EXIT,
                ));
            }
        }
    }

    let close_started = Instant::now();
    let close_result = close_write_session(&mut write_client, &session_id).await;
    let close_ms = duration_ms(close_started.elapsed());
    if let Some(last_record) = object_records.last_mut() {
        last_record["close_ms"] = json!(close_ms);
    }
    for record in object_records {
        print_json_line(record)?;
    }
    print_json_line(write_many_summary_json(
        &args.pool,
        args.count,
        bytes_per_object,
        args.count,
        None,
        Some(close_ms),
        close_result.as_ref().err().map(ToString::to_string),
    ))?;
    close_result?;
    Ok(())
}

async fn open_write_session(
    channel: Channel,
    client: &mut pb::write_session_service_client::WriteSessionServiceClient<Channel>,
    pool: &str,
    library_uuid: Vec<u8>,
    no_wait: bool,
) -> AppResult<Vec<u8>> {
    let request = || pb::OpenWriteSessionRequest {
        target: Some(pb::open_write_session_request::Target::PoolTarget(
            pb::TapePoolTarget {
                pool_id: pool.trim().to_string(),
                library_uuid: library_uuid.clone(),
                mount_if_needed: true,
            },
        )),
        body_format: "rao-v1".to_string(),
        idempotency_key: None,
        recover_session_id: Vec::new(),
    };
    match client.open_write_session(request()).await {
        Ok(response) => Ok(response.into_inner().session_id),
        Err(status) if !no_wait => {
            wait_before_open_retry(channel, &status).await?;
            Ok(client
                .open_write_session(request())
                .await?
                .into_inner()
                .session_id)
        }
        Err(status) => Err(status.into()),
    }
}

async fn close_write_session(
    client: &mut pb::write_session_service_client::WriteSessionServiceClient<Channel>,
    session_id: &[u8],
) -> AppResult<()> {
    client
        .close_write_session(pb::CloseWriteSessionRequest {
            session_id: session_id.to_vec(),
            idempotency_key: None,
        })
        .await?;
    Ok(())
}

#[derive(Clone, Debug)]
struct AppendInput {
    caller_object_id: String,
    archive_path: String,
    declared_size_bytes: u64,
    chunk_bytes: usize,
    start_digest: Option<[u8; 32]>,
    replay_from_start: bool,
    source: AppendSource,
}

#[derive(Clone, Debug)]
enum AppendSource {
    File(PathBuf),
    Generated { object_index: u64 },
}

impl AppendInput {
    fn file(
        file: &Path,
        chunk_bytes: usize,
        declared_size_bytes: u64,
        content_sha256: Option<[u8; 32]>,
    ) -> Self {
        let archive_path = file
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or("payload.bin")
            .to_string();
        Self {
            caller_object_id: Uuid::new_v4().to_string(),
            archive_path,
            declared_size_bytes,
            chunk_bytes,
            start_digest: content_sha256,
            replay_from_start: content_sha256.is_some(),
            source: AppendSource::File(file.to_path_buf()),
        }
    }

    fn generated(
        object_index: u64,
        declared_size_bytes: u64,
        chunk_bytes: usize,
        caller_object_id: String,
    ) -> Self {
        Self {
            caller_object_id,
            archive_path: format!("generated-object-{object_index}.bin"),
            declared_size_bytes,
            chunk_bytes,
            start_digest: None,
            replay_from_start: false,
            source: AppendSource::Generated { object_index },
        }
    }
}

async fn append_object(
    client: &mut pb::write_session_service_client::WriteSessionServiceClient<Channel>,
    session_id: &[u8],
    input: AppendInput,
) -> AppResult<pb::ObjectRecord> {
    let (tx, rx) = tokio::sync::mpsc::channel::<pb::AppendObjectMessage>(8);
    let session_id_for_task = session_id.to_vec();

    let sender = tokio::spawn(async move {
        let mut caller_metadata = HashMap::new();
        caller_metadata.insert("path".to_string(), input.archive_path);
        let start_digest = input.start_digest;
        tx.send(pb::AppendObjectMessage {
            payload: Some(pb::append_object_message::Payload::Start(
                pb::AppendObjectStart {
                    session_id: session_id_for_task.clone(),
                    caller_object_id: input.caller_object_id,
                    caller_metadata,
                    declared_size_bytes: input.declared_size_bytes,
                    body_format_manifest: Vec::new(),
                    expected_content_sha256: start_digest
                        .map(|digest| digest.to_vec())
                        .unwrap_or_default(),
                    expected_content_digest: start_digest.map(|digest| pb::Digest {
                        algorithm: "sha256".to_string(),
                        value: digest.to_vec(),
                    }),
                    source_replay_capability: if input.replay_from_start {
                        pb::SourceReplayCapability::ReplayFromStart as i32
                    } else {
                        pb::SourceReplayCapability::Unspecified as i32
                    },
                },
            )),
        })
        .await
        .map_err(|_| AppError::new("append stream closed before Start"))?;

        let mut hasher = Sha256::new();
        match input.source {
            AppendSource::File(file) => {
                send_file_chunks(
                    &tx,
                    &session_id_for_task,
                    &file,
                    input.chunk_bytes,
                    &mut hasher,
                )
                .await?;
            }
            AppendSource::Generated { object_index } => {
                send_generated_chunks(
                    &tx,
                    &session_id_for_task,
                    object_index,
                    input.declared_size_bytes,
                    input.chunk_bytes,
                    &mut hasher,
                )
                .await?;
            }
        }

        let finish_digest = start_digest
            .map(|digest| digest.to_vec())
            .unwrap_or_else(|| hasher.finalize().to_vec());
        tx.send(pb::AppendObjectMessage {
            payload: Some(pb::append_object_message::Payload::Finish(
                pb::AppendObjectFinish {
                    session_id: session_id_for_task,
                    expected_content_sha256: finish_digest.clone(),
                    expected_content_digest: Some(pb::Digest {
                        algorithm: "sha256".to_string(),
                        value: finish_digest,
                    }),
                },
            )),
        })
        .await
        .map_err(|_| AppError::new("append stream closed before Finish"))?;
        Ok::<(), AppError>(())
    });

    let append = client.append_object(ReceiverStream::new(rx)).await;
    let sender_result = sender
        .await
        .map_err(|error| AppError::new(format!("append sender task failed: {error}")))?;
    map_append_completion(append.map(|response| response.into_inner()), sender_result)
}

async fn hash_file_before_start(path: &Path) -> AppResult<[u8; 32]> {
    let mut input = tokio::fs::File::open(path)
        .await
        .map_err(|error| AppError::new(format!("open {} for hashing: {error}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; DEFAULT_CHUNK_BYTES];
    loop {
        let read = input
            .read(&mut buffer)
            .await
            .map_err(|error| AppError::new(format!("hash {}: {error}", path.display())))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

fn map_append_completion<T>(
    append: Result<T, tonic::Status>,
    sender_result: AppResult<()>,
) -> AppResult<T> {
    match (append, sender_result) {
        (Ok(response), Ok(())) => Ok(response),
        (Ok(response), Err(error)) if error.is_append_stream_closed() => Ok(response),
        (Ok(_), Err(error)) => Err(error),
        (Err(status), Ok(())) => Err(AppError::from(status)),
        (Err(status), Err(error)) if error.is_append_stream_closed() => Err(AppError::from(status)),
        (Err(_), Err(error)) => Err(error),
    }
}

async fn send_file_chunks(
    tx: &tokio::sync::mpsc::Sender<pb::AppendObjectMessage>,
    session_id: &[u8],
    file: &Path,
    chunk_bytes: usize,
    hasher: &mut Sha256,
) -> AppResult<()> {
    let mut input = tokio::fs::File::open(file)
        .await
        .map_err(|error| AppError::new(format!("open {}: {error}", file.display())))?;
    let mut buffer = vec![0_u8; chunk_bytes];
    loop {
        let n = input
            .read(&mut buffer)
            .await
            .map_err(|error| AppError::new(format!("read {}: {error}", file.display())))?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        send_append_chunk(tx, session_id, buffer[..n].to_vec()).await?;
    }
    Ok(())
}

async fn send_generated_chunks(
    tx: &tokio::sync::mpsc::Sender<pb::AppendObjectMessage>,
    session_id: &[u8],
    object_index: u64,
    total_bytes: u64,
    chunk_bytes: usize,
    hasher: &mut Sha256,
) -> AppResult<()> {
    let mut offset = 0_u64;
    while offset < total_bytes {
        let n = std::cmp::min(chunk_bytes as u64, total_bytes - offset) as usize;
        let mut data = vec![0_u8; n];
        fill_generated_payload_chunk(object_index, offset, &mut data);
        hasher.update(&data);
        send_append_chunk(tx, session_id, data).await?;
        offset = offset.saturating_add(n as u64);
    }
    Ok(())
}

async fn send_append_chunk(
    tx: &tokio::sync::mpsc::Sender<pb::AppendObjectMessage>,
    session_id: &[u8],
    data: Vec<u8>,
) -> AppResult<()> {
    tx.send(pb::AppendObjectMessage {
        payload: Some(pb::append_object_message::Payload::Chunk(
            pb::AppendObjectChunk {
                session_id: session_id.to_vec(),
                data,
            },
        )),
    })
    .await
    .map_err(|_| AppError::new("append stream closed while sending Chunk"))
}

fn fill_generated_payload_chunk(object_index: u64, offset: u64, data: &mut [u8]) {
    let object_index_bytes = object_index.to_le_bytes();
    for (idx, byte) in data.iter_mut().enumerate() {
        let absolute = offset.saturating_add(idx as u64);
        *byte = if absolute < object_index_bytes.len() as u64 {
            object_index_bytes[absolute as usize]
        } else {
            let word = splitmix64(object_index ^ (absolute / 8));
            word.to_le_bytes()[(absolute % 8) as usize]
        };
    }
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

async fn abort_write_session(
    client: &mut pb::write_session_service_client::WriteSessionServiceClient<Channel>,
    session_id: &[u8],
) {
    let _ = client
        .abort_write_session(pb::AbortWriteSessionRequest {
            session_id: session_id.to_vec(),
            idempotency_key: None,
            reason: "remfield-io append failed".to_string(),
        })
        .await;
}

async fn read_command(endpoint: &str, args: ReadArgs, no_wait: bool) -> AppResult<()> {
    let channel = connect_daemon(endpoint).await?;
    let mut catalog_client = pb::catalog_client::CatalogClient::new(channel.clone());
    let target =
        resolve_read_target(&mut catalog_client, &args.object, args.offset, args.length).await?;
    let mut read_client =
        pb::read_session_service_client::ReadSessionServiceClient::new(channel.clone());

    let started = Instant::now();
    let open_started = Instant::now();
    let request = || pb::OpenReadSessionRequest {
        target: Some(pb::open_read_session_request::Target::TapeTarget(
            pb::TapeTarget {
                tape_uuid: target.tape_uuid.to_vec(),
                mount_if_needed: true,
                required_pool_id: target.required_pool_id.clone(),
            },
        )),
        idempotency_key: None,
        resume_target: None,
    };
    let session = match read_client.open_read_session(request()).await {
        Ok(response) => response.into_inner(),
        Err(status) if !no_wait => {
            wait_before_open_retry(channel, &status).await?;
            read_client.open_read_session(request()).await?.into_inner()
        }
        Err(status) => return Err(status.into()),
    };
    let open_ms = duration_ms(open_started.elapsed());

    let transfer_started = Instant::now();
    let read_result =
        read_object_range(&mut read_client, &session.session_id, &target, &args).await;
    let transfer_ms = duration_ms(transfer_started.elapsed());
    let close_started = Instant::now();
    let close_result = read_client
        .close_read_session(pb::CloseReadSessionRequest {
            session_id: session.session_id,
            idempotency_key: None,
        })
        .await;
    let close_ms = duration_ms(close_started.elapsed());

    let (bytes, sha256) = read_result?;
    close_result?;

    let seconds = started.elapsed().as_secs_f64();
    print_json_line(json!({
        "object_id": target.object_id.to_string(),
        "tape_uuid": bytes_to_hex(&target.tape_uuid),
        "pool_id": empty_string_as_null(&target.required_pool_id),
        "bytes": bytes,
        "seconds": seconds,
        "mb_s": mb_s(bytes, seconds),
        "open_ms": open_ms,
        "transfer_ms": transfer_ms,
        "close_ms": close_ms,
        "mib_per_s": mib_per_s(bytes, transfer_ms),
        "sha256": sha256,
    }))?;
    Ok(())
}

async fn read_object_range(
    client: &mut pb::read_session_service_client::ReadSessionServiceClient<Channel>,
    session_id: &[u8],
    target: &ReadTarget,
    args: &ReadArgs,
) -> AppResult<(u64, String)> {
    let temp_out = temporary_output_path(&args.out)?;
    let mut output = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_out)
        .await
        .map_err(|error| AppError::new(format!("create {}: {error}", temp_out.display())))?;
    let mut stream = client
        .read_object_range(pb::ReadObjectRangeRequest {
            session_id: session_id.to_vec(),
            object_id: target.object_id.as_bytes().to_vec(),
            file_id: Vec::new(),
            start_byte: target.start_byte,
            end_byte: target.end_byte,
            stream_chunk_bytes: args.chunk_bytes,
        })
        .await?
        .into_inner();

    let mut hasher = Sha256::new();
    let mut bytes = 0_u64;
    while let Some(chunk) = stream.message().await? {
        if !chunk.data.is_empty() {
            output
                .write_all(&chunk.data)
                .await
                .map_err(|error| AppError::new(format!("write {}: {error}", temp_out.display())))?;
            bytes = bytes.saturating_add(chunk.data.len() as u64);
            hasher.update(&chunk.data);
        }
    }
    output
        .flush()
        .await
        .map_err(|error| AppError::new(format!("flush {}: {error}", temp_out.display())))?;
    drop(output);
    tokio::fs::rename(&temp_out, &args.out)
        .await
        .map_err(|error| {
            AppError::new(format!(
                "install {} over {}: {error}",
                temp_out.display(),
                args.out.display()
            ))
        })?;
    Ok((bytes, bytes_to_hex(&hasher.finalize())))
}

async fn list_command(endpoint: &str, args: ListArgs) -> AppResult<()> {
    let channel = connect_daemon(endpoint).await?;
    let mut catalog_client = pb::catalog_client::CatalogClient::new(channel);
    let tapes = catalog_client
        .list_tapes(pb::ListTapesRequest {
            library_uuid: Vec::new(),
            page_token: None,
            page_size: 0,
            pool_id: args.pool.clone(),
            kind: "data".to_string(),
        })
        .await?
        .into_inner()
        .tapes;

    let mut object_stream = catalog_client
        .enumerate_objects(pb::EnumerateObjectsRequest {
            scope: Some(pb::enumerate_objects_request::Scope::All(())),
            reconcile_from_tape: false,
        })
        .await?
        .into_inner();
    let mut objects = Vec::new();
    let mut counts_by_tape: BTreeMap<String, u64> = BTreeMap::new();
    while let Some(object) = object_stream.message().await? {
        for copy in object
            .copies
            .iter()
            .filter(|copy| copy.pool_id == args.pool)
        {
            let tape_uuid = bytes_to_hex(&copy.tape_uuid);
            *counts_by_tape.entry(tape_uuid.clone()).or_default() += 1;
            objects.push(json!({
                "object_id": uuid_bytes_to_text(&object.object_id)?,
                "caller_object_id": empty_string_as_null(&object.caller_object_id),
                "content_sha256": bytes_to_hex(&object.content_sha256),
                "logical_size_bytes": object.logical_size_bytes,
                "body_format": object.body_format,
                "tape_uuid": tape_uuid,
                "tape_file_number": copy.tape_file_number,
                "first_body_lba": copy.first_body_lba,
                "pool_id": copy.pool_id,
                "health": copy.health,
            }));
        }
    }

    let tape_values = tapes
        .iter()
        .map(|tape| {
            let tape_uuid = bytes_to_hex(&tape.tape_uuid);
            json!({
                "tape_uuid": tape_uuid,
                "voltag": empty_string_as_null(&tape.voltag),
                "pool_id": empty_string_as_null(&tape.pool_id),
                "body_format": empty_string_as_null(&tape.body_format),
                "block_size_bytes": tape.block_size_bytes,
                "last_committed_tape_file": tape.last_committed_tape_file,
                "state": tape_state_name(tape.state),
                "object_count": counts_by_tape.get(&bytes_to_hex(&tape.tape_uuid)).copied().unwrap_or(0),
            })
        })
        .collect::<Vec<_>>();

    print_json_line(json!({
        "pool": args.pool,
        "tape_count": tape_values.len(),
        "object_count": objects.len(),
        "tapes": tape_values,
        "objects": objects,
    }))?;
    Ok(())
}

async fn resolve_library_uuid(channel: Channel, serial: Option<&str>) -> AppResult<Vec<u8>> {
    let Some(serial) = serial.map(str::trim).filter(|serial| !serial.is_empty()) else {
        return Ok(Vec::new());
    };
    let mut client = pb::library_service_client::LibraryServiceClient::new(channel);
    let response = client.list_libraries(()).await?.into_inner();
    response
        .libraries
        .into_iter()
        .find(|library| library.library_serial == serial)
        .map(|library| library.library_uuid)
        .ok_or_else(|| AppError::new(format!("library serial {serial:?} not found by daemon")))
}

#[derive(Debug)]
struct ReadTarget {
    object_id: Uuid,
    tape_uuid: [u8; 16],
    required_pool_id: String,
    start_byte: u64,
    end_byte: u64,
}

#[derive(Debug)]
struct ObjectSpec {
    object_id: Uuid,
    tape_uuid: Option<[u8; 16]>,
    pool_id: Option<String>,
}

#[derive(Deserialize)]
struct JsonObjectSpec {
    object_id: String,
    tape_uuid: Option<String>,
    pool_id: Option<String>,
}

async fn resolve_read_target(
    catalog_client: &mut pb::catalog_client::CatalogClient<Channel>,
    raw: &str,
    offset: u64,
    length: Option<u64>,
) -> AppResult<ReadTarget> {
    let spec = parse_object_spec(raw)?;
    let needs_catalog = spec.tape_uuid.is_none() || (offset > 0 && length.is_none());
    let object = if needs_catalog {
        Some(fetch_object(catalog_client, spec.object_id).await?)
    } else {
        None
    };

    let tape_uuid = match (spec.tape_uuid, object.as_ref()) {
        (Some(tape_uuid), _) => tape_uuid,
        (None, Some(object)) => object
            .copies
            .first()
            .map(|copy| uuid_slice(&copy.tape_uuid, "copy.tape_uuid"))
            .transpose()?
            .ok_or_else(|| AppError::new("object has no cataloged copies"))?,
        (None, None) => return Err(AppError::new("missing tape_uuid and catalog object")),
    };

    let required_pool_id = match (spec.pool_id, object.as_ref()) {
        (Some(pool_id), _) => pool_id,
        (None, Some(object)) => object
            .copies
            .iter()
            .find(|copy| copy.tape_uuid == tape_uuid)
            .map(|copy| copy.pool_id.clone())
            .unwrap_or_default(),
        (None, None) => String::new(),
    };

    let (start_byte, end_byte) = read_range(offset, length, object.as_ref())?;
    Ok(ReadTarget {
        object_id: spec.object_id,
        tape_uuid,
        required_pool_id,
        start_byte,
        end_byte,
    })
}

async fn fetch_object(
    catalog_client: &mut pb::catalog_client::CatalogClient<Channel>,
    object_id: Uuid,
) -> AppResult<pb::ObjectRecord> {
    Ok(catalog_client
        .get_object(pb::GetObjectRequest {
            key: Some(pb::get_object_request::Key::ObjectId(
                object_id.as_bytes().to_vec(),
            )),
        })
        .await?
        .into_inner())
}

fn parse_object_spec(raw: &str) -> AppResult<ObjectSpec> {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') {
        let spec: JsonObjectSpec = serde_json::from_str(trimmed)
            .map_err(|error| AppError::new(format!("parse --object locator json: {error}")))?;
        return Ok(ObjectSpec {
            object_id: Uuid::parse_str(spec.object_id.trim()).map_err(|error| {
                AppError::new(format!("parse object_id {:?}: {error}", spec.object_id))
            })?,
            tape_uuid: spec
                .tape_uuid
                .as_deref()
                .map(parse_uuid_or_hex)
                .transpose()?,
            pool_id: spec.pool_id.filter(|pool| !pool.trim().is_empty()),
        });
    }

    Ok(ObjectSpec {
        object_id: Uuid::parse_str(trimmed)
            .map_err(|error| AppError::new(format!("parse --object UUID {trimmed:?}: {error}")))?,
        tape_uuid: None,
        pool_id: None,
    })
}

fn read_range(
    offset: u64,
    length: Option<u64>,
    object: Option<&pb::ObjectRecord>,
) -> AppResult<(u64, u64)> {
    match (offset, length) {
        (0, None) => Ok((0, 0)),
        (start, Some(len)) => Ok((
            start,
            start
                .checked_add(len)
                .ok_or_else(|| AppError::new("--offset + --length overflows u64"))?,
        )),
        (start, None) => {
            let size = object
                .map(|object| object.logical_size_bytes)
                .ok_or_else(|| {
                    AppError::new("--offset without --length requires catalog lookup")
                })?;
            if start > size {
                return Err(AppError::new(format!(
                    "--offset {start} is beyond object size {size}"
                )));
            }
            Ok((start, size))
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct PhaseTimings {
    open_ms: Option<f64>,
    transfer_ms: Option<f64>,
    close_ms: Option<f64>,
}

fn write_result_json(
    record: &pb::ObjectRecord,
    pool_id: &str,
    bytes: u64,
    seconds: f64,
    timings: PhaseTimings,
) -> Value {
    let first_copy = record.copies.first();
    json!({
        "object_id": uuid_bytes_to_text(&record.object_id).unwrap_or_else(|_| bytes_to_hex(&record.object_id)),
        "caller_object_id": empty_string_as_null(&record.caller_object_id),
        "tape_uuid": first_copy.map(|copy| bytes_to_hex(&copy.tape_uuid)),
        "tape_file_number": first_copy.map(|copy| copy.tape_file_number),
        "first_body_lba": first_copy.map(|copy| copy.first_body_lba),
        "content_sha256": bytes_to_hex(&record.content_sha256),
        "pool_id": pool_id,
        "body_format": record.body_format,
        "logical_size_bytes": record.logical_size_bytes,
        "bytes": bytes,
        "seconds": seconds,
        "mb_s": mb_s(bytes, seconds),
        "open_ms": timings.open_ms,
        "transfer_ms": timings.transfer_ms,
        "close_ms": timings.close_ms,
        "mib_per_s": timings.transfer_ms.map(|transfer_ms| mib_per_s(bytes, transfer_ms)),
        "append_commit_info": append_commit_info_json(record.append_commit_info.as_ref()),
    })
}

fn write_many_object_json(
    record: &pb::ObjectRecord,
    pool_id: &str,
    object_index: u64,
    bytes: u64,
    timings: PhaseTimings,
) -> Value {
    let transfer_seconds = timings
        .transfer_ms
        .map(|value| value / 1000.0)
        .unwrap_or_default();
    let mut value = write_result_json(record, pool_id, bytes, transfer_seconds, timings);
    value["record_type"] = json!("object");
    value["object_index"] = json!(object_index);
    value
}

fn write_many_error_json(
    object_index: u64,
    pool_id: &str,
    bytes: u64,
    caller_object_id: &str,
    error: String,
    timings: PhaseTimings,
) -> Value {
    json!({
        "record_type": "object",
        "object_index": object_index,
        "caller_object_id": caller_object_id,
        "pool_id": pool_id,
        "bytes": bytes,
        "open_ms": timings.open_ms,
        "transfer_ms": timings.transfer_ms,
        "close_ms": timings.close_ms,
        "mib_per_s": timings.transfer_ms.map(|transfer_ms| mib_per_s(bytes, transfer_ms)),
        "append_commit_info": Value::Null,
        "error": error,
    })
}

fn write_many_summary_json(
    pool_id: &str,
    requested_count: u64,
    bytes_per_object: u64,
    committed_count: u64,
    failed_index: Option<u64>,
    close_ms: Option<f64>,
    close_error: Option<String>,
) -> Value {
    let prefix_committed = failed_index.map(|idx| {
        if idx == 0 {
            "no objects were committed before the failed append".to_string()
        } else {
            format!("objects 0..{} remain committed", idx - 1)
        }
    });
    json!({
        "record_type": "summary",
        "pool_id": pool_id,
        "requested_count": requested_count,
        "bytes_per_object": bytes_per_object,
        "committed_count": committed_count,
        "failed_index": failed_index,
        "prefix_committed": prefix_committed,
        "close_ms": close_ms,
        "close_error": close_error,
        "ok": failed_index.is_none() && close_error.is_none(),
    })
}

fn append_commit_info_json(info: Option<&pb::AppendCommitInfo>) -> Value {
    match info {
        Some(info) => json!({
            "append_mode": append_mode_name(info.append_mode),
            "tape_uuid": bytes_to_hex(&info.tape_uuid),
            "voltag": info.voltag.as_deref(),
            "tape_file_number": info.tape_file_number,
            "first_body_lba": info.first_body_lba,
            "position_before_lba": info.position_before_lba,
            "position_after_lba": info.position_after_lba,
            "journal_record_ordinal": info.journal_record_ordinal,
            "estimated_remaining_bytes": info.estimated_remaining_bytes,
            "sealed_after_write": info.sealed_after_write,
        }),
        None => Value::Null,
    }
}

fn append_mode_name(value: i32) -> &'static str {
    match pb::AppendMode::try_from(value).unwrap_or(pb::AppendMode::Unspecified) {
        pb::AppendMode::Fresh => "fresh",
        pb::AppendMode::Append => "append",
        pb::AppendMode::ResumeControl => "resume_control",
        pb::AppendMode::Seal => "seal",
        pb::AppendMode::Unspecified => "unspecified",
    }
}

fn parse_uuid_or_hex(value: &str) -> AppResult<[u8; 16]> {
    let trimmed = value.trim();
    if trimmed.len() == 32 && trimmed.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        let bytes = hex_to_bytes(trimmed)?;
        return bytes
            .as_slice()
            .try_into()
            .map_err(|_| AppError::new(format!("UUID hex must be 16 bytes: {trimmed:?}")));
    }
    Ok(*Uuid::parse_str(trimmed)
        .map_err(|error| AppError::new(format!("parse UUID {trimmed:?}: {error}")))?
        .as_bytes())
}

fn uuid_slice(bytes: &[u8], label: &str) -> AppResult<[u8; 16]> {
    bytes
        .try_into()
        .map_err(|_| AppError::new(format!("{label} must be 16 bytes, got {}", bytes.len())))
}

fn uuid_bytes_to_text(bytes: &[u8]) -> AppResult<String> {
    Ok(Uuid::from_bytes(uuid_slice(bytes, "uuid")?).to_string())
}

fn hex_to_bytes(value: &str) -> AppResult<Vec<u8>> {
    if value.len() % 2 != 0 {
        return Err(AppError::new(format!(
            "hex string has odd length {}",
            value.len()
        )));
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .enumerate()
        .map(|(idx, pair)| {
            let high = hex_nibble(pair[0])
                .ok_or_else(|| AppError::new(format!("invalid hex at byte {}", idx * 2)))?;
            let low = hex_nibble(pair[1])
                .ok_or_else(|| AppError::new(format!("invalid hex at byte {}", idx * 2 + 1)))?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(char::from(HEX[(byte >> 4) as usize]));
        out.push(char::from(HEX[(byte & 0x0f) as usize]));
    }
    out
}

fn mb_s(bytes: u64, seconds: f64) -> f64 {
    if seconds > 0.0 {
        (bytes as f64 / seconds) / (1024.0 * 1024.0)
    } else {
        0.0
    }
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn mib_per_s(bytes: u64, transfer_ms: f64) -> f64 {
    if transfer_ms > 0.0 {
        (bytes as f64 / (transfer_ms / 1000.0)) / (1024.0 * 1024.0)
    } else {
        0.0
    }
}

fn empty_string_as_null(value: &str) -> Option<&str> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn tape_state_name(state: i32) -> String {
    pb::tape::State::try_from(state)
        .map(|state| format!("{state:?}"))
        .unwrap_or_else(|_| format!("UNKNOWN({state})"))
}

fn temporary_output_path(out: &Path) -> AppResult<PathBuf> {
    let file_name = out
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| AppError::new(format!("invalid --out path {}", out.display())))?;
    let temp_name = format!(".{file_name}.remfield-io.{}.tmp", std::process::id());
    Ok(out.with_file_name(temp_name))
}

fn print_json_line(value: Value) -> AppResult<()> {
    let line = serde_json::to_string(&value)
        .map_err(|error| AppError::new(format!("serialize json result: {error}")))?;
    println!("{line}");
    Ok(())
}

fn print_json_error(message: String) {
    let line = serde_json::to_string(&json!({ "error": message }))
        .unwrap_or_else(|_| "{\"error\":\"failed to serialize error\"}".to_string());
    eprintln!("{line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_wait_is_a_plain_global_flag() {
        let cli = Cli::try_parse_from([
            "remfield-io",
            "write-many",
            "--pool",
            "copy-a",
            "--count",
            "1",
            "--size-mib",
            "1",
            "--caller-object-id-prefix",
            "field",
            "--no-wait",
        ])
        .expect("parse --no-wait after subcommand arguments");

        assert!(cli.no_wait);
    }

    #[test]
    fn write_serial_flag_selects_legacy_ab_path() {
        let cli = Cli::try_parse_from([
            "remfield-io",
            "write",
            "--file",
            "/tmp/payload.bin",
            "--pool",
            "copy-a",
            "--serial",
        ])
        .expect("parse write --serial");
        let Command::Write(write) = cli.command else {
            panic!("expected write command");
        };
        assert!(write.serial);
    }

    #[test]
    fn readiness_fence_parser_accepts_new_and_existing_fence_wording() {
        let operation_id = Uuid::from_u128(0x42);
        let created = format!(
            "open write session blocked by media-readiness fence operation={operation_id} library=LIBMAIN"
        );
        let existing = format!(
            "open read session blocked by active media-readiness fence operation={operation_id} state=becoming_ready"
        );

        assert_eq!(media_readiness_operation_id(&created), Some(operation_id));
        assert_eq!(media_readiness_operation_id(&existing), Some(operation_id));
        assert_eq!(
            media_readiness_operation_id(&format!("unrelated operation={operation_id}")),
            None
        );
    }

    #[test]
    fn drive_busy_retry_is_narrowly_classified() {
        assert!(drive_bay_busy("drive bay 0x0100 is busy"));
        assert!(drive_bay_busy("drive-session owner is busy"));
        assert!(!drive_bay_busy("all drives are busy"));
        assert!(!drive_bay_busy("drive bay 0x0100 is fenced"));
    }

    #[test]
    fn write_result_json_surfaces_append_commit_info() {
        let record = pb::ObjectRecord {
            object_id: Uuid::nil().as_bytes().to_vec(),
            caller_object_id: "caller-object".to_string(),
            content_sha256: vec![0x22; 32],
            logical_size_bytes: 64,
            body_format: "rao-v1".to_string(),
            caller_metadata: Default::default(),
            created_at: None,
            content_digest: Some(pb::Digest {
                algorithm: "sha256".to_string(),
                value: vec![0x22; 32],
            }),
            metadata_digest: None,
            copies: vec![pb::ObjectCopy {
                tape_uuid: vec![0x44; 16],
                tape_file_number: 3,
                first_body_lba: 9,
                last_verified_at: None,
                health: pb::object_copy::Health::ObjectCopyHealthOk as i32,
                pool_id: "camera.copy-a".to_string(),
                plaintext_digest: Some(pb::Digest {
                    algorithm: "sha256".to_string(),
                    value: vec![0x22; 32],
                }),
                stored_digest: None,
            }],
            append_commit_info: Some(pb::AppendCommitInfo {
                append_mode: pb::AppendMode::Append as i32,
                tape_uuid: vec![0x44; 16],
                voltag: None,
                tape_file_number: 3,
                first_body_lba: 9,
                position_before_lba: None,
                position_after_lba: None,
                journal_record_ordinal: None,
                estimated_remaining_bytes: None,
                sealed_after_write: None,
            }),
        };

        let value = write_result_json(
            &record,
            "camera.copy-a",
            64,
            2.0,
            PhaseTimings {
                open_ms: Some(1.5),
                transfer_ms: Some(2000.0),
                close_ms: Some(2.5),
            },
        );
        let info = &value["append_commit_info"];
        assert_eq!(info["append_mode"].as_str().unwrap(), "append");
        assert_eq!(
            info["tape_uuid"].as_str().unwrap(),
            "44444444444444444444444444444444"
        );
        assert!(info["voltag"].is_null());
        assert_eq!(info["tape_file_number"].as_u64().unwrap(), 3);
        assert_eq!(info["first_body_lba"].as_u64().unwrap(), 9);
        assert!(info["position_before_lba"].is_null());
        assert!(info["position_after_lba"].is_null());
        assert!(info["journal_record_ordinal"].is_null());
        assert!(info["estimated_remaining_bytes"].is_null());
        assert!(info["sealed_after_write"].is_null());
        assert_eq!(value["open_ms"].as_f64().unwrap(), 1.5);
        assert_eq!(value["transfer_ms"].as_f64().unwrap(), 2000.0);
        assert_eq!(value["close_ms"].as_f64().unwrap(), 2.5);
        assert_eq!(
            value["mib_per_s"].as_f64().unwrap(),
            64.0 / 2.0 / 1024.0 / 1024.0
        );
    }

    #[test]
    fn append_mode_name_maps_all_known_values() {
        assert_eq!(
            append_mode_name(pb::AppendMode::Unspecified as i32),
            "unspecified"
        );
        assert_eq!(append_mode_name(pb::AppendMode::Fresh as i32), "fresh");
        assert_eq!(append_mode_name(pb::AppendMode::Append as i32), "append");
        assert_eq!(
            append_mode_name(pb::AppendMode::ResumeControl as i32),
            "resume_control"
        );
        assert_eq!(append_mode_name(pb::AppendMode::Seal as i32), "seal");
        assert_eq!(append_mode_name(i32::MAX), "unspecified");
    }

    #[test]
    fn write_many_object_json_carries_phase_timing_shape() {
        let record = pb::ObjectRecord {
            object_id: Uuid::nil().as_bytes().to_vec(),
            caller_object_id: "batch-7".to_string(),
            content_sha256: vec![0x33; 32],
            logical_size_bytes: 1024 * 1024,
            body_format: "rao-v1".to_string(),
            caller_metadata: Default::default(),
            created_at: None,
            content_digest: Some(pb::Digest {
                algorithm: "sha256".to_string(),
                value: vec![0x33; 32],
            }),
            metadata_digest: None,
            copies: vec![pb::ObjectCopy {
                tape_uuid: vec![0x55; 16],
                tape_file_number: 8,
                first_body_lba: 99,
                last_verified_at: None,
                health: pb::object_copy::Health::ObjectCopyHealthOk as i32,
                pool_id: "fieldtest-a".to_string(),
                plaintext_digest: Some(pb::Digest {
                    algorithm: "sha256".to_string(),
                    value: vec![0x33; 32],
                }),
                stored_digest: None,
            }],
            append_commit_info: Some(pb::AppendCommitInfo {
                append_mode: pb::AppendMode::Append as i32,
                tape_uuid: vec![0x55; 16],
                voltag: Some("AOX030L9".to_string()),
                tape_file_number: 8,
                first_body_lba: 99,
                position_before_lba: None,
                position_after_lba: None,
                journal_record_ordinal: None,
                estimated_remaining_bytes: None,
                sealed_after_write: None,
            }),
        };

        let value = write_many_object_json(
            &record,
            "fieldtest-a",
            7,
            1024 * 1024,
            PhaseTimings {
                open_ms: None,
                transfer_ms: Some(500.0),
                close_ms: Some(3.0),
            },
        );

        assert_eq!(value["record_type"], "object");
        assert_eq!(value["object_index"], 7);
        assert!(value["open_ms"].is_null());
        assert_eq!(value["transfer_ms"].as_f64().unwrap(), 500.0);
        assert_eq!(value["close_ms"].as_f64().unwrap(), 3.0);
        assert_eq!(value["bytes"], 1024 * 1024);
        assert_eq!(value["mib_per_s"].as_f64().unwrap(), 2.0);
        assert_eq!(value["append_commit_info"]["append_mode"], "append");
    }

    #[test]
    fn append_completion_prefers_rpc_status_over_stream_closed_sender_error() {
        let status = tonic::Status::resource_exhausted(
            "create append spool in /ram/spool: no space left on device",
        );
        let err = map_append_completion::<()>(
            Err(status),
            Err(AppError::new("append stream closed while sending Chunk")),
        )
        .expect_err("rpc status should surface");

        assert!(err
            .to_string()
            .contains("create append spool in /ram/spool"));
        assert!(!err.to_string().contains("append stream closed"));
    }

    #[test]
    fn append_completion_accepts_early_success_for_idempotent_replay() {
        let result = map_append_completion(
            Ok("committed-object"),
            Err(AppError::new("append stream closed while sending Chunk")),
        )
        .expect("an early successful replay response is authoritative");
        assert_eq!(result, "committed-object");
    }

    #[test]
    fn append_completion_keeps_local_sender_errors_when_no_channel_close() {
        let err = map_append_completion::<()>(
            Err(tonic::Status::internal("daemon status")),
            Err(AppError::new("read /payload.bin: input/output error")),
        )
        .expect_err("local read error should surface");

        assert!(err.to_string().contains("read /payload.bin"));
        assert!(!err.to_string().contains("daemon status"));
    }

    #[test]
    fn generated_payload_chunks_include_object_identity() {
        let mut first = vec![0_u8; 64];
        let mut second = vec![0_u8; 64];
        fill_generated_payload_chunk(1, 0, &mut first);
        fill_generated_payload_chunk(2, 0, &mut second);

        assert_ne!(first, second);
        assert_eq!(&first[..8], &1_u64.to_le_bytes());
        assert_eq!(&second[..8], &2_u64.to_le_bytes());
    }
}
