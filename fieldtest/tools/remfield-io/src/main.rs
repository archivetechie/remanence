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
use std::time::Instant;

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

type AppResult<T> = Result<T, AppError>;

#[derive(Debug)]
struct AppError(String);

impl AppError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}

impl From<tonic::Status> for AppError {
    fn from(status: tonic::Status) -> Self {
        Self(status.to_string())
    }
}

impl From<tonic::transport::Error> for AppError {
    fn from(error: tonic::transport::Error) -> Self {
        Self(error.to_string())
    }
}

#[derive(Parser, Debug)]
#[command(name = "remfield-io")]
#[command(about = "Field-test daemon write/read helper")]
struct Cli {
    #[arg(long, global = true, default_value = DEFAULT_ENDPOINT)]
    endpoint: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    Write(WriteArgs),
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
            print_json_error(error.to_string());
            ExitCode::from(1)
        }
    }
}

async fn run(cli: Cli) -> AppResult<()> {
    match cli.command {
        Command::Write(args) => write_command(&cli.endpoint, args).await,
        Command::Read(args) => read_command(&cli.endpoint, args).await,
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

async fn write_command(endpoint: &str, args: WriteArgs) -> AppResult<()> {
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

    let channel = connect_daemon(endpoint).await?;
    let library_uuid = resolve_library_uuid(channel.clone(), args.library.as_deref()).await?;
    let mut write_client =
        pb::write_session_service_client::WriteSessionServiceClient::new(channel);

    let started = Instant::now();
    let session = write_client
        .open_write_session(pb::OpenWriteSessionRequest {
            target: Some(pb::open_write_session_request::Target::PoolTarget(
                pb::TapePoolTarget {
                    pool_id: args.pool.trim().to_string(),
                    library_uuid,
                    mount_if_needed: true,
                },
            )),
            body_format: "rao-v1".to_string(),
            idempotency_key: None,
            recover_session_id: Vec::new(),
        })
        .await?
        .into_inner();

    let append_result = append_file(&mut write_client, &session.session_id, &args).await;
    let record = match append_result {
        Ok(record) => record,
        Err(error) => {
            abort_write_session(&mut write_client, &session.session_id).await;
            return Err(error);
        }
    };

    write_client
        .close_write_session(pb::CloseWriteSessionRequest {
            session_id: session.session_id.clone(),
            idempotency_key: None,
        })
        .await?;

    let seconds = started.elapsed().as_secs_f64();
    let bytes = metadata.len();
    print_json_line(write_result_json(&record, &args.pool, bytes, seconds))?;
    Ok(())
}

async fn append_file(
    client: &mut pb::write_session_service_client::WriteSessionServiceClient<Channel>,
    session_id: &[u8],
    args: &WriteArgs,
) -> AppResult<pb::ObjectRecord> {
    let (tx, rx) = tokio::sync::mpsc::channel::<pb::AppendObjectMessage>(8);
    let session_id_for_task = session_id.to_vec();
    let file = args.file.clone();
    let chunk_bytes = args.chunk_bytes;
    let declared_size_bytes = tokio::fs::metadata(&args.file).await?.len();
    let caller_object_id = Uuid::new_v4().to_string();
    let archive_path = args
        .file
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("payload.bin")
        .to_string();

    let sender = tokio::spawn(async move {
        let mut caller_metadata = HashMap::new();
        caller_metadata.insert("path".to_string(), archive_path);
        tx.send(pb::AppendObjectMessage {
            payload: Some(pb::append_object_message::Payload::Start(
                pb::AppendObjectStart {
                    session_id: session_id_for_task.clone(),
                    caller_object_id,
                    caller_metadata,
                    declared_size_bytes,
                    body_format_manifest: Vec::new(),
                },
            )),
        })
        .await
        .map_err(|_| AppError::new("append stream closed before Start"))?;

        let mut input = tokio::fs::File::open(&file)
            .await
            .map_err(|error| AppError::new(format!("open {}: {error}", file.display())))?;
        let mut buffer = vec![0_u8; chunk_bytes];
        let mut hasher = Sha256::new();
        loop {
            let n = input
                .read(&mut buffer)
                .await
                .map_err(|error| AppError::new(format!("read {}: {error}", file.display())))?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
            tx.send(pb::AppendObjectMessage {
                payload: Some(pb::append_object_message::Payload::Chunk(
                    pb::AppendObjectChunk {
                        session_id: session_id_for_task.clone(),
                        data: buffer[..n].to_vec(),
                    },
                )),
            })
            .await
            .map_err(|_| AppError::new("append stream closed while sending Chunk"))?;
        }

        tx.send(pb::AppendObjectMessage {
            payload: Some(pb::append_object_message::Payload::Finish(
                pb::AppendObjectFinish {
                    session_id: session_id_for_task,
                    expected_content_sha256: hasher.finalize().to_vec(),
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
    sender_result?;
    Ok(append?.into_inner())
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

async fn read_command(endpoint: &str, args: ReadArgs) -> AppResult<()> {
    let channel = connect_daemon(endpoint).await?;
    let mut catalog_client = pb::catalog_client::CatalogClient::new(channel.clone());
    let target =
        resolve_read_target(&mut catalog_client, &args.object, args.offset, args.length).await?;
    let mut read_client = pb::read_session_service_client::ReadSessionServiceClient::new(channel);

    let started = Instant::now();
    let session = read_client
        .open_read_session(pb::OpenReadSessionRequest {
            target: Some(pb::open_read_session_request::Target::TapeTarget(
                pb::TapeTarget {
                    tape_uuid: target.tape_uuid.to_vec(),
                    mount_if_needed: true,
                    required_pool_id: target.required_pool_id.clone(),
                },
            )),
            idempotency_key: None,
        })
        .await?
        .into_inner();

    let read_result =
        read_object_range(&mut read_client, &session.session_id, &target, &args).await;
    let close_result = read_client
        .close_read_session(pb::CloseReadSessionRequest {
            session_id: session.session_id,
            idempotency_key: None,
        })
        .await;

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

fn write_result_json(record: &pb::ObjectRecord, pool_id: &str, bytes: u64, seconds: f64) -> Value {
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
    })
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
