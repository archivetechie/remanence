//! Pool-targeted direct archive tape helpers.
//!
//! This module implements:
//! - `rem-debug archive write` — write one file to a pool-selected tape
//!   (hardware op; requires `--library` + `--allow`).
//!
//! Tape provisioning (`rem tape init`) lives in the main CLI under the
//! new design's `Tape::Init` handler; this module deliberately holds
//! no provisioning logic.

use std::collections::HashSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::bytes_to_hex;
use remanence_aead::{RecipientPrivateKey, RecipientPublicKey, RootKey};
use remanence_api::{
    load_tape_by_uuid,
    read_core::{read_object_payload, CapturePayloadSink},
    select_tape_in_pool, verify_tape_identity, write_to_selected_tape, PoolWriteObjectRecord,
    PoolWriteRepresentation, PoolWriteResult, TapeIdentityError, TapeUuid,
    WriteObjectToPoolRequest,
};
use remanence_format::{
    read_envelope_rao_object_with_manifest_anchor, RemTarReadObject, MANIFEST_PATH,
};
use remanence_library::{
    BlockSize, BlockSource, DriveHandleSink, DriveHandleSource, SpaceKind, StaticAllowlist,
    TapeConfig,
};
use remanence_state::{
    CatalogIndex, NativeObjectCopyRecord, StateHandle, TapeFileRecord,
    OBJECT_COPY_REPRESENTATION_ENCRYPTED, OBJECT_COPY_REPRESENTATION_PLAINTEXT,
};
use sha2::Digest;
use uuid::Uuid;
use zeroize::Zeroize;

// =====================================================================
//  archive write
// =====================================================================

/// Arguments for `rem archive write`.
pub struct ArchiveWriteArgs {
    /// Library serial to allow.
    pub library: String,
    /// Local file to write to the tape pool.
    pub file: PathBuf,
    /// Pool id to target.
    pub pool_id: String,
    /// Override in-archive path (default: file basename).
    pub archive_path: Option<PathBuf>,
    /// Opaque caller/orchestrator object id (optional; default: new UUID).
    pub caller_object_id: Option<String>,
    /// Store the RAO1 encrypted representation instead of plaintext rao-v1.
    pub encrypt: bool,
    /// 32-byte root key file for encrypted writes.
    pub key_file: Option<PathBuf>,
    /// 16-byte key identifier as 32 hex characters.
    pub key_id: Option<String>,
    /// Canonical RAOR recipient public-key files.
    pub recipients: Vec<PathBuf>,
    /// Emit locator JSON to stdout instead of human-readable form.
    pub json_output: bool,
    /// Path to config file.
    pub config: PathBuf,
}

/// Run `rem archive write`.
pub fn run_archive_write(
    report: &remanence_library::DiscoveryReport,
    args: &ArchiveWriteArgs,
    allow: &[String],
    allow_derived: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    // -- Validate inputs before any I/O ----------------------------------
    if !args.file.exists() {
        let _ = writeln!(err, "error: --file {} does not exist", args.file.display());
        return ExitCode::from(1);
    }
    let object_size = match std::fs::metadata(&args.file) {
        Ok(m) => m.len(),
        Err(e) => {
            let _ = writeln!(err, "error: stat {}: {e}", args.file.display());
            return ExitCode::from(1);
        }
    };

    let pool_id = args.pool_id.trim().to_string();
    if pool_id.is_empty() {
        let _ = writeln!(err, "error: --pool must not be empty");
        return ExitCode::from(1);
    }

    let archive_path = match &args.archive_path {
        Some(p) => p.clone(),
        None => args
            .file
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("payload.bin")),
    };

    let caller_object_id = args
        .caller_object_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let representation = match write_representation_from_args(args, err) {
        Ok(representation) => representation,
        Err(code) => return code,
    };

    // -- Open state -------------------------------------------------------
    let mut state_handle = match StateHandle::open_from_config_file(&args.config) {
        Ok(h) => h,
        Err(e) => {
            let _ = writeln!(err, "error: open state: {e}");
            return ExitCode::from(1);
        }
    };

    // -- Resolve the pool config (caller-supplied policy, watermarks, etc.) --
    let pool_cfg = match state_handle
        .config()
        .tape_pools
        .iter()
        .find(|p| p.id.trim() == pool_id)
        .cloned()
    {
        Some(cfg) => cfg,
        None => {
            let _ = writeln!(err, "error: unknown tape pool: {pool_id}");
            return ExitCode::from(1);
        }
    };

    // -- Select tape (catalog-only, no hardware) --------------------------
    // No live reservations on a one-shot CLI invocation.
    let reserved: HashSet<TapeUuid> = HashSet::new();
    let selected = {
        let index = state_handle.catalog_index();
        match select_tape_in_pool(index, &pool_cfg, object_size, &reserved) {
            Ok(s) => s,
            Err(e) => {
                let _ = writeln!(err, "error: select tape in pool {pool_id}: {e}");
                if let remanence_api::SelectTapeError::NoWritableTapes { reasons, .. } = &e {
                    for (i, r) in reasons.iter().enumerate() {
                        let _ = writeln!(err, "  rejection {}: {r}", i + 1);
                    }
                }
                return ExitCode::from(1);
            }
        }
    };

    let tape_uuid = selected.tape_uuid;
    let block_size = selected.block_size;

    // -- Open library handle ----------------------------------------------
    let lib = match report.library(&args.library) {
        Some(l) => l,
        None => {
            let _ = writeln!(
                err,
                "error: no library with serial {:?} on this host",
                args.library
            );
            let _ = writeln!(err, "       run `rem libraries` to see what's available");
            return ExitCode::from(2);
        }
    };

    let mut policy = StaticAllowlist::new(allow.iter().cloned());
    for s in allow_derived {
        policy = policy.with_derived_allowed(s.clone());
    }

    let mut library_handle = match crate::open_library_handle(lib, &policy) {
        Ok(h) => h,
        Err(e) => {
            let _ = writeln!(err, "error: opening library: {e}");
            return ExitCode::from(1);
        }
    };

    // -- Load tape by UUID ------------------------------------------------
    let mut drive = {
        let index = state_handle.catalog_index();
        match load_tape_by_uuid(index, &mut library_handle, &policy, &tape_uuid) {
            Ok(d) => d,
            Err(e) => {
                let _ = writeln!(err, "error: load tape: {e}");
                return ExitCode::from(1);
            }
        }
    };

    // -- Verify-before-write: read bootstrap at BOT -----------------------
    if let Err(e) = drive.rewind() {
        let _ = writeln!(err, "error: rewind before verify: {e}");
        return ExitCode::from(1);
    }
    {
        let mut source = DriveHandleSource(&mut drive);
        match verify_tape_identity(&mut source, &tape_uuid) {
            Ok(()) => {}
            Err(TapeIdentityError::AbsentBootstrap(msg)) => {
                let _ = writeln!(
                    err,
                    "error: tape identity check failed (no bootstrap at BOT): {msg}"
                );
                let _ = writeln!(
                    err,
                    "       run `rem tape init` to initialise this cartridge first"
                );
                return ExitCode::from(1);
            }
            Err(TapeIdentityError::Mismatch { expected, actual }) => {
                let _ = writeln!(
                    err,
                    "error: tape identity mismatch — expected {expected}, on-tape bootstrap says {actual}"
                );
                let _ = writeln!(
                    err,
                    "       the wrong cartridge may be loaded; aborting to prevent data loss"
                );
                return ExitCode::from(1);
            }
        }
    }

    // -- Rewind + fixed-block config + write object -----------------------
    if let Err(e) = drive.rewind() {
        let _ = writeln!(err, "error: rewind before write: {e}");
        return ExitCode::from(1);
    }
    let current_cfg = match drive.read_config() {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(err, "error: read drive config before write: {e}");
            return ExitCode::from(1);
        }
    };
    if let Err(e) = drive.write_config(TapeConfig {
        block_size: BlockSize::Fixed {
            size_bytes: block_size,
        },
        compression: false,
        max_block_size_bytes: current_cfg.max_block_size_bytes,
        write_protected: current_cfg.write_protected,
        worm: current_cfg.worm,
    }) {
        let _ = writeln!(err, "error: set fixed-block config: {e}");
        return ExitCode::from(1);
    }

    let request = WriteObjectToPoolRequest {
        pool_id: pool_id.clone(),
        source_path: args.file.clone(),
        archive_path,
        caller_object_id,
        expected_content_sha256: None,
        representation: representation.representation,
    };

    let result = {
        let mut sink = DriveHandleSink(&mut drive);
        write_to_selected_tape(
            state_handle.catalog_index(),
            &mut sink,
            &pool_cfg,
            request,
            selected,
        )
    };

    match result {
        Ok(PoolWriteResult { object, .. }) => {
            if args.json_output {
                print_locator_json_with_recipients(
                    &object,
                    &pool_id,
                    representation.recipient_epochs.as_deref(),
                    out,
                );
            } else {
                print_locator_human(&object, &pool_id, out);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            let _ = writeln!(err, "error: write object: {e}");
            ExitCode::from(1)
        }
    }
}

struct WriteRepresentationSelection {
    representation: PoolWriteRepresentation,
    recipient_epochs: Option<Vec<serde_json::Value>>,
}

fn write_representation_from_args(
    args: &ArchiveWriteArgs,
    err: &mut dyn Write,
) -> Result<WriteRepresentationSelection, ExitCode> {
    if !args.recipients.is_empty() {
        if args.encrypt || args.key_file.is_some() || args.key_id.is_some() {
            let _ = writeln!(
                err,
                "error: --recipient cannot be combined with --encrypt/--key-file/--key-id"
            );
            return Err(ExitCode::from(1));
        }
        let recipients = read_recipient_files(&args.recipients).map_err(|error| {
            let _ = writeln!(err, "error: {error}");
            ExitCode::from(1)
        })?;
        let recipient_epochs = recipients
            .iter()
            .map(|recipient| {
                serde_json::json!({
                    "epoch_id": bytes_to_hex(&recipient.recipient_epoch_id),
                    "label": recipient.epoch_label,
                })
            })
            .collect();
        return Ok(WriteRepresentationSelection {
            representation: PoolWriteRepresentation::Encrypted { recipients },
            recipient_epochs: Some(recipient_epochs),
        });
    }
    if !args.encrypt {
        if args.key_file.is_some() || args.key_id.is_some() {
            let _ = writeln!(err, "error: --key-file/--key-id require --encrypt");
            return Err(ExitCode::from(1));
        }
        return Ok(WriteRepresentationSelection {
            representation: PoolWriteRepresentation::Plaintext,
            recipient_epochs: None,
        });
    }

    let key_file = match args.key_file.as_deref() {
        Some(path) => path,
        None => {
            let _ = writeln!(err, "error: --encrypt requires --key-file");
            return Err(ExitCode::from(1));
        }
    };
    let key_id = match args.key_id.as_deref().map(parse_key_id) {
        Some(Ok(key_id)) => key_id,
        Some(Err(error)) => {
            let _ = writeln!(err, "error: --key-id: {error}");
            return Err(ExitCode::from(1));
        }
        None => {
            let _ = writeln!(err, "error: --encrypt requires --key-id");
            return Err(ExitCode::from(1));
        }
    };
    let root_key = match read_root_key_file(key_file) {
        Ok(root_key) => root_key,
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            return Err(ExitCode::from(1));
        }
    };
    drop(root_key);
    let _ = key_id;
    let _ = writeln!(
        err,
        "error: registry-symmetric writes are retired; use two or more --recipient files"
    );
    Err(ExitCode::from(1))
}

fn read_recipient_files(paths: &[PathBuf]) -> Result<Vec<RecipientPublicKey>, String> {
    if !(2..=8).contains(&paths.len()) {
        return Err("--recipient must be repeated 2 to 8 times".to_string());
    }
    let mut recipients = Vec::with_capacity(paths.len());
    for path in paths {
        let bytes = std::fs::read(path)
            .map_err(|error| format!("read --recipient {}: {error}", path.display()))?;
        recipients.push(
            RecipientPublicKey::parse(&bytes)
                .map_err(|error| format!("parse --recipient {}: {error}", path.display()))?,
        );
    }
    if recipients
        .windows(2)
        .any(|pair| pair[0].slot_index >= pair[1].slot_index)
        || recipients.iter().enumerate().any(|(index, recipient)| {
            recipients[..index]
                .iter()
                .any(|earlier| earlier.recipient_epoch_id == recipient.recipient_epoch_id)
        })
    {
        return Err("--recipient epochs must be distinct and in ascending slot order".to_string());
    }
    Ok(recipients)
}

fn read_root_key_file(path: &Path) -> Result<RootKey, String> {
    let mut bytes = std::fs::read(path)
        .map_err(|error| format!("read --key-file {}: {error}", path.display()))?;
    if bytes.len() != 32 {
        let len = bytes.len();
        bytes.zeroize();
        return Err(format!(
            "--key-file must contain exactly 32 bytes, got {len}"
        ));
    }
    RootKey::new(bytes).map_err(|error| error.to_string())
}

fn read_private_key_file(path: &Path) -> Result<RecipientPrivateKey, String> {
    let mut bytes = std::fs::read(path)
        .map_err(|error| format!("read --private-key {}: {error}", path.display()))?;
    let parsed = RecipientPrivateKey::parse(&bytes)
        .map_err(|error| format!("parse --private-key {}: {error}", path.display()));
    bytes.zeroize();
    parsed
}

fn parse_key_id(value: &str) -> Result<[u8; 16], String> {
    let bytes = hex_to_bytes(value)?;
    <[u8; 16]>::try_from(bytes)
        .map_err(|bytes| format!("key id must decode to 16 bytes, got {}", bytes.len()))
}

// =====================================================================
//  Locator output
// =====================================================================

/// Print the locator as a single JSON line to stdout (§4 contract).
#[cfg(test)]
pub(crate) fn print_locator_json(
    object: &PoolWriteObjectRecord,
    pool_id: &str,
    out: &mut dyn Write,
) {
    print_locator_json_with_recipients(object, pool_id, None, out);
}

fn print_locator_json_with_recipients(
    object: &PoolWriteObjectRecord,
    pool_id: &str,
    recipient_epochs: Option<&[serde_json::Value]>,
    out: &mut dyn Write,
) {
    let copy = match object.copies.first() {
        Some(c) => c,
        None => return,
    };
    let tape_uuid_hex = bytes_to_hex(&copy.tape_uuid);
    let object_id = Uuid::from_bytes(object.object_id).to_string();
    let content_sha256_hex = bytes_to_hex(&object.content_sha256);
    let encryption = if copy.representation == OBJECT_COPY_REPRESENTATION_ENCRYPTED {
        "RAO1"
    } else {
        "none"
    };
    let append_mode = append_mode_name_for_tape_file_number(copy.tape_file_number);

    // §4: single compact line on stdout — no pretty-printing.
    let json = serde_json::json!({
        "tape_uuid": tape_uuid_hex.as_str(),
        "tape_file_number": copy.tape_file_number,
        "first_body_lba": copy.first_body_lba,
        "object_id": object_id,
        "caller_object_id": object.caller_object_id,
        "content_sha256": content_sha256_hex,
        "pool_id": pool_id,
        "body_format": "rao-v1",
        "representation": copy.representation.as_str(),
        "encryption": encryption,
        "format_version": (copy.representation == OBJECT_COPY_REPRESENTATION_ENCRYPTED)
            .then_some(2),
        "recipient_epochs": recipient_epochs,
        "metadata_frame_len": copy.metadata_frame_len,
        "append_commit_info": {
            "append_mode": append_mode,
            "tape_uuid": tape_uuid_hex.as_str(),
            "voltag": serde_json::Value::Null,
            "tape_file_number": copy.tape_file_number,
            "first_body_lba": copy.first_body_lba,
            "position_before_lba": serde_json::Value::Null,
            "position_after_lba": serde_json::Value::Null,
            "journal_record_ordinal": serde_json::Value::Null,
            "estimated_remaining_bytes": serde_json::Value::Null,
            "sealed_after_write": serde_json::Value::Null,
        },
    });
    let line = serde_json::to_string(&json).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
    let _ = writeln!(out, "{line}");
}

fn append_mode_name_for_tape_file_number(tape_file_number: u64) -> &'static str {
    match tape_file_number {
        0 => "unspecified",
        1 => "fresh",
        _ => "append",
    }
}

/// Print the locator in human-readable form.
fn print_locator_human(object: &PoolWriteObjectRecord, pool_id: &str, out: &mut dyn Write) {
    let copy = match object.copies.first() {
        Some(c) => c,
        None => return,
    };
    let tape_uuid_hex = bytes_to_hex(&copy.tape_uuid);
    let object_id = Uuid::from_bytes(object.object_id).to_string();
    let content_sha256_hex = bytes_to_hex(&object.content_sha256);
    let encryption = if copy.representation == OBJECT_COPY_REPRESENTATION_ENCRYPTED {
        "RAO1"
    } else {
        "none"
    };

    let _ = writeln!(out, "ok: archive write committed");
    let _ = writeln!(out, "  pool:           {pool_id}");
    let _ = writeln!(out, "  tape_uuid:      {tape_uuid_hex}");
    let _ = writeln!(out, "  tape_file_num:  {}", copy.tape_file_number);
    let _ = writeln!(out, "  first_body_lba: {}", copy.first_body_lba);
    let _ = writeln!(
        out,
        "  append_mode:    {}",
        append_mode_name_for_tape_file_number(copy.tape_file_number)
    );
    let _ = writeln!(out, "  object_id:      {object_id}");
    let _ = writeln!(out, "  caller_obj_id:  {}", object.caller_object_id);
    let _ = writeln!(out, "  content_sha256: {content_sha256_hex}");
    let _ = writeln!(out, "  logical_bytes:  {}", object.logical_size_bytes);
    let _ = writeln!(out, "  body_format:    rao-v1");
    let _ = writeln!(out, "  representation: {}", copy.representation);
    let _ = writeln!(out, "  encryption:     {encryption}");
    if let Some(recipient_epoch_ids) = &copy.recipient_epoch_ids {
        let _ = writeln!(out, "  recipient_ids:  {}", recipient_epoch_ids.join(","));
    }
    if let Some(metadata_frame_len) = copy.metadata_frame_len {
        let _ = writeln!(out, "  metadata_frame: {metadata_frame_len}");
    }
}

// =====================================================================
//  archive read (A.9) — design-verification skeleton
//
//  Per rust-design-verification: this is a compiling skeleton. The
//  borrow-sensitive orchestration `run_archive_read` is written for real
//  (so the borrow checker validates the DriveHandle/BlockSource plumbing —
//  Category 4); the mechanical helpers and sink bodies are filled in below.
// =====================================================================

/// Canonical JSON locator as emitted by `archive write --json` (§4 contract).
#[derive(serde::Deserialize)]
pub struct ObjectLocator {
    pub tape_uuid: String,
    pub tape_file_number: u32,
    pub first_body_lba: u64,
    pub object_id: String,
    pub caller_object_id: Option<String>,
    pub content_sha256: String,
    pub pool_id: Option<String>,
    pub body_format: Option<String>,
}

/// Locator decoded into rem's byte-typed identity fields.
struct DecodedLocator {
    tape_uuid: TapeUuid,
    object_id: String,
    tape_file_number: u32,
    first_body_lba: u64,
    content_sha256: [u8; 32],
}

/// What the catalog must supply that the locator omits: how many fixed blocks
/// to read, and at what block size.
struct ObjectReadPlan {
    block_count: u64,
    block_size_bytes: u32,
    manifest_sha256: Option<[u8; 32]>,
    representation: String,
    recipient_epoch_ids: Option<Vec<String>>,
    metadata_frame_len: Option<u64>,
}

/// Arguments for `rem-debug archive read`.
pub struct ArchiveReadArgs {
    /// Library serial to allow.
    pub library: String,
    /// Canonical locator JSON (the A.5 `--json` line).
    pub locator: String,
    /// Destination path for the restored payload bytes.
    pub out: PathBuf,
    /// 32-byte root key file for encrypted object copies.
    pub key_file: Option<PathBuf>,
    /// Canonical RAOP private-key file for encrypted object copies.
    pub private_key: Option<PathBuf>,
    /// Path to config file.
    pub config: PathBuf,
}

/// Arguments for `rem-debug archive export-object`.
pub struct ArchiveExportObjectArgs {
    /// Library serial to allow.
    pub library: String,
    /// Canonical locator JSON emitted by `archive write --json`.
    pub locator: String,
    /// Destination path for the complete stored object bytes.
    pub out: PathBuf,
    /// Path to config file.
    pub config: PathBuf,
}

/// Arguments for `rem-debug archive verify`.
pub struct ArchiveVerifyArgs {
    /// Library serial to allow.
    pub library: String,
    /// Canonical locator JSON emitted by `archive write --json`.
    pub locator: String,
    /// Expected payload SHA-256, hex (the catalog's recorded asset hash).
    pub expected_sha256: String,
    /// 32-byte root key file for encrypted object copies.
    pub key_file: Option<PathBuf>,
    /// Canonical RAOP private-key file for encrypted object copies.
    pub private_key: Option<PathBuf>,
    /// Path to config file.
    pub config: PathBuf,
}

/// One-line JSON receipt printed on success.
#[derive(serde::Serialize)]
struct ArchiveReadReceipt {
    object_id: String,
    bytes_written: u64,
    content_sha256: String,
    verified: bool,
}

#[derive(serde::Serialize)]
struct ArchiveExportObjectReceipt {
    object_id: String,
    bytes_written: u64,
    block_count: u64,
    block_size_bytes: u32,
    representation: String,
    object_sha256: String,
}

/// One-line JSON receipt printed by `archive verify`.
#[derive(serde::Serialize)]
struct ArchiveVerifyReceipt {
    verified: bool,
    expected_sha256: String,
    actual_sha256: String,
}

/// Hex-decode a `2*N`-char string into `N` bytes.
fn hex_to_bytes(s: &str) -> Result<Vec<u8>, String> {
    let bytes = s.as_bytes();
    if bytes.len() % 2 != 0 {
        return Err(format!("hex string has odd length {}", bytes.len()));
    }

    fn nibble(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }

    bytes
        .chunks_exact(2)
        .enumerate()
        .map(|(i, pair)| {
            let high =
                nibble(pair[0]).ok_or_else(|| format!("invalid hex byte at offset {}", i * 2))?;
            let low = nibble(pair[1])
                .ok_or_else(|| format!("invalid hex byte at offset {}", i * 2 + 1))?;
            Ok((high << 4) | low)
        })
        .collect()
}

/// Parse + hex-decode the canonical locator into byte-typed fields.
fn decode_locator(raw: &str) -> Result<DecodedLocator, String> {
    let loc: ObjectLocator =
        serde_json::from_str(raw).map_err(|e| format!("parse locator json: {e}"))?;
    let _locator_metadata = (&loc.caller_object_id, &loc.pool_id, &loc.body_format);

    let tape_uuid_bytes = hex_to_bytes(&loc.tape_uuid)?;
    let tape_uuid: TapeUuid = tape_uuid_bytes
        .as_slice()
        .try_into()
        .map_err(|_| format!("tape_uuid must be 16 bytes, got {}", tape_uuid_bytes.len()))?;

    let sha_bytes = hex_to_bytes(&loc.content_sha256)?;
    let content_sha256: [u8; 32] = sha_bytes
        .as_slice()
        .try_into()
        .map_err(|_| format!("content_sha256 must be 32 bytes, got {}", sha_bytes.len()))?;

    Ok(DecodedLocator {
        tape_uuid,
        object_id: loc.object_id,
        tape_file_number: loc.tape_file_number,
        first_body_lba: loc.first_body_lba,
        content_sha256,
    })
}

/// Pure: derive the read plan from already-fetched catalog records,
/// validating the copy location against the locator.
fn plan_from_records(
    copies: &[NativeObjectCopyRecord],
    tape_files: &[TapeFileRecord],
    block_size: Option<u64>,
    manifest_sha256: Option<[u8; 32]>,
    loc: &DecodedLocator,
) -> Result<ObjectReadPlan, String> {
    let copy = copies
        .iter()
        .find(|c| {
            c.object_id == loc.object_id
                && c.tape_uuid.as_slice() == loc.tape_uuid.as_slice()
                && c.tape_file_number == loc.tape_file_number
                && c.first_body_lba == loc.first_body_lba
        })
        .ok_or_else(|| {
            format!(
                "no catalog copy of object {} at tape_file {} lba {}",
                loc.object_id, loc.tape_file_number, loc.first_body_lba
            )
        })?;
    let recipient_epoch_ids = copy.recipient_epoch_ids.clone();
    match copy.representation.as_str() {
        OBJECT_COPY_REPRESENTATION_PLAINTEXT => {
            if recipient_epoch_ids.is_some() || copy.metadata_frame_len.is_some() {
                return Err("plaintext catalog copy carries encrypted envelope fields".to_string());
            }
        }
        OBJECT_COPY_REPRESENTATION_ENCRYPTED => {
            if recipient_epoch_ids.as_ref().is_none_or(Vec::is_empty) {
                return Err("encrypted catalog copy is missing recipient_epoch_ids".to_string());
            }
            if copy.metadata_frame_len.is_none() {
                return Err("encrypted catalog copy is missing metadata_frame_len".to_string());
            }
        }
        other => {
            return Err(format!("unsupported object copy representation {other:?}"));
        }
    }

    let tape_file = tape_files
        .iter()
        .find(|f| {
            f.kind == "object"
                && f.tape_uuid.as_slice() == loc.tape_uuid.as_slice()
                && f.tape_file_number == loc.tape_file_number
                && f.object_id.as_deref() == Some(loc.object_id.as_str())
        })
        .ok_or_else(|| {
            format!(
                "no object tape file {} for object {}",
                loc.tape_file_number, loc.object_id
            )
        })?;

    let block_size_bytes =
        u32::try_from(block_size.ok_or_else(|| "tape has no recorded block size".to_string())?)
            .map_err(|_| "tape block size exceeds u32".to_string())?;

    Ok(ObjectReadPlan {
        block_count: tape_file.block_count,
        block_size_bytes,
        manifest_sha256,
        representation: copy.representation.clone(),
        recipient_epoch_ids,
        metadata_frame_len: copy.metadata_frame_len,
    })
}

/// Resolve the object's body block count + block size from the catalog,
/// validating the copy location matches the locator. The locator carries no
/// length, so this read is mandatory before any tape I/O — and is done while
/// the catalog borrow is live, returning owned values so no borrow is held
/// across the drive's lifetime (Category 4).
fn resolve_object_read_plan(
    index: &CatalogIndex,
    loc: &DecodedLocator,
) -> Result<ObjectReadPlan, String> {
    let object = index
        .get_native_object(&loc.object_id)
        .map_err(|e| format!("catalog: {e}"))?;
    let object = object.ok_or_else(|| format!("object {} not found in catalog", loc.object_id))?;
    let manifest_sha256 = object
        .metadata_hash
        .as_deref()
        .map(|hash| {
            <[u8; 32]>::try_from(hash)
                .map_err(|_| format!("metadata_hash must be 32 bytes, got {}", hash.len()))
        })
        .transpose()?;
    let tape_files = index
        .list_tape_files(&loc.tape_uuid)
        .map_err(|e| format!("catalog: {e}"))?;
    let block_size = index
        .get_tape(&loc.tape_uuid)
        .map_err(|e| format!("catalog: {e}"))?
        .and_then(|t| t.block_size);
    plan_from_records(
        &object.copies,
        &tape_files,
        block_size,
        manifest_sha256,
        loc,
    )
}

/// The catalog/library identity of one object to stream (read + verify share it).
struct TapeObjectRef<'a> {
    library: &'a str,
    config: &'a Path,
    locator_json: &'a str,
    key_file: Option<&'a Path>,
    private_key: Option<&'a Path>,
}

/// Mounted, identity-checked, fixed-block-configured tape object.
struct MountedTapeObject {
    loc: DecodedLocator,
    plan: ObjectReadPlan,
    _library_handle: remanence_library::LibraryHandle,
    drive: remanence_library::DriveHandle,
}

/// Result of streaming one object's payload through a sink.
struct TapeStreamOutcome {
    object_id: String,
    locator_content_sha256: [u8; 32],
    payload_bytes: u64,
    actual_sha256: [u8; 32],
}

/// Result of exporting one object's stored fixed blocks.
struct StoredObjectExportOutcome {
    object_id: String,
    block_count: u64,
    block_size_bytes: u32,
    representation: String,
    bytes_written: u64,
    object_sha256: [u8; 32],
}

fn mount_tape_object(
    report: &remanence_library::DiscoveryReport,
    target: &TapeObjectRef<'_>,
    allow: &[String],
    allow_derived: &[String],
    err: &mut dyn Write,
) -> Result<MountedTapeObject, ExitCode> {
    let loc = decode_locator(target.locator_json).map_err(|e| {
        let _ = writeln!(err, "error: locator: {e}");
        ExitCode::from(1)
    })?;

    let mut state_handle = StateHandle::open_from_config_file(target.config).map_err(|e| {
        let _ = writeln!(err, "error: open state: {e}");
        ExitCode::from(1)
    })?;

    let plan = {
        let index = state_handle.catalog_index();
        resolve_object_read_plan(index, &loc).map_err(|e| {
            let _ = writeln!(err, "error: locate object in catalog: {e}");
            ExitCode::from(1)
        })?
    };

    let lib = report.library(target.library).ok_or_else(|| {
        let _ = writeln!(err, "error: no library with serial {:?}", target.library);
        ExitCode::from(2)
    })?;
    let mut policy = StaticAllowlist::new(allow.iter().cloned());
    for s in allow_derived {
        policy = policy.with_derived_allowed(s.clone());
    }
    let mut library_handle = crate::open_library_handle(lib, &policy).map_err(|e| {
        let _ = writeln!(err, "error: opening library: {e}");
        ExitCode::from(1)
    })?;

    let mut drive = {
        let index = state_handle.catalog_index();
        load_tape_by_uuid(index, &mut library_handle, &policy, &loc.tape_uuid).map_err(|e| {
            let _ = writeln!(err, "error: load tape: {e}");
            ExitCode::from(1)
        })?
    };

    drive.rewind().map_err(|e| {
        let _ = writeln!(err, "error: rewind before verify: {e}");
        ExitCode::from(1)
    })?;
    {
        let mut source = DriveHandleSource(&mut drive);
        verify_tape_identity(&mut source, &loc.tape_uuid).map_err(|e| {
            let _ = writeln!(err, "error: tape identity: {e}");
            ExitCode::from(1)
        })?;
    }

    let current_cfg = drive.read_config().map_err(|e| {
        let _ = writeln!(err, "error: read drive config: {e}");
        ExitCode::from(1)
    })?;
    drive
        .write_config(TapeConfig {
            block_size: BlockSize::Fixed {
                size_bytes: plan.block_size_bytes,
            },
            compression: false,
            max_block_size_bytes: current_cfg.max_block_size_bytes,
            write_protected: current_cfg.write_protected,
            worm: current_cfg.worm,
        })
        .map_err(|e| {
            let _ = writeln!(err, "error: set fixed-block config: {e}");
            ExitCode::from(1)
        })?;

    Ok(MountedTapeObject {
        loc,
        plan,
        _library_handle: library_handle,
        drive,
    })
}

/// Mount, position, and stream one object's payload through `sink_writer`,
/// returning the streamed hash + byte count.
fn stream_tape_object<W: Write + Send>(
    report: &remanence_library::DiscoveryReport,
    target: &TapeObjectRef<'_>,
    allow: &[String],
    allow_derived: &[String],
    sink_writer: W,
    err: &mut dyn Write,
) -> Result<TapeStreamOutcome, ExitCode> {
    let mut mounted = mount_tape_object(report, target, allow, allow_derived, err)?;

    let (payload_bytes, actual_sha256) = match mounted.plan.representation.as_str() {
        OBJECT_COPY_REPRESENTATION_PLAINTEXT => {
            if target.key_file.is_some() || target.private_key.is_some() {
                let _ = writeln!(
                    err,
                    "error: key options are only valid for encrypted copies"
                );
                return Err(ExitCode::from(1));
            }
            let mut sink = CapturePayloadSink::new(sink_writer);
            let stream_result = {
                let mut source = DriveHandleSource(&mut mounted.drive);
                read_object_payload(
                    &mut source,
                    mounted.plan.block_size_bytes as usize,
                    mounted.plan.block_count,
                    mounted.loc.tape_file_number,
                    mounted.plan.manifest_sha256,
                    &mut sink,
                )
            };
            if let Err(e) = stream_result {
                let _ = writeln!(err, "error: read object: {e}");
                return Err(ExitCode::from(1));
            }
            sink.finish().map_err(|e| {
                let _ = writeln!(err, "error: {e}");
                ExitCode::from(1)
            })?
        }
        OBJECT_COPY_REPRESENTATION_ENCRYPTED => {
            if target.key_file.is_some() {
                let _ = writeln!(err, "error: v2 encrypted copies require --private-key");
                return Err(ExitCode::from(1));
            }
            let private_key = target.private_key.ok_or_else(|| {
                let _ = writeln!(err, "error: encrypted copy requires --private-key");
                ExitCode::from(1)
            })?;
            let recipient = read_private_key_file(private_key).map_err(|error| {
                let _ = writeln!(err, "error: {error}");
                ExitCode::from(1)
            })?;
            let opened = {
                let mut source = DriveHandleSource(&mut mounted.drive);
                if let Err(error) = source.space(
                    i64::from(mounted.loc.tape_file_number),
                    remanence_library::SpaceKind::Filemarks,
                ) {
                    let _ = writeln!(err, "error: space to object tape file: {error}");
                    return Err(ExitCode::from(1));
                }
                read_envelope_rao_object_with_manifest_anchor(
                    &mut source,
                    mounted.plan.block_size_bytes as usize,
                    mounted.plan.block_count,
                    &recipient,
                    mounted.plan.manifest_sha256,
                )
            }
            .map_err(|error| {
                let _ = writeln!(err, "error: open encrypted RAO: {error}");
                ExitCode::from(1)
            })?;
            let opened_recipient_epoch_ids = opened
                .envelope
                .key_frame
                .as_ref()
                .expect("v2 open report carries key frame")
                .slots
                .iter()
                .map(|slot| bytes_to_hex(&slot.recipient_epoch_id))
                .collect::<Vec<_>>();
            if Some(opened_recipient_epoch_ids) != mounted.plan.recipient_epoch_ids {
                let _ = writeln!(
                    err,
                    "error: encrypted RAO recipient epochs differ from catalog"
                );
                return Err(ExitCode::from(1));
            }
            if Some(opened.envelope.header.metadata_frame_len) != mounted.plan.metadata_frame_len {
                let _ = writeln!(
                    err,
                    "error: encrypted RAO metadata_frame_len differs from catalog"
                );
                return Err(ExitCode::from(1));
            }
            write_single_payload_from_read_object(&opened.object, sink_writer).map_err(|error| {
                let _ = writeln!(err, "error: {error}");
                ExitCode::from(1)
            })?
        }
        other => {
            let _ = writeln!(
                err,
                "error: unsupported object copy representation {other:?}"
            );
            return Err(ExitCode::from(1));
        }
    };

    Ok(TapeStreamOutcome {
        object_id: mounted.loc.object_id,
        locator_content_sha256: mounted.loc.content_sha256,
        payload_bytes,
        actual_sha256,
    })
}

fn stream_stored_tape_object<W: Write>(
    report: &remanence_library::DiscoveryReport,
    target: &TapeObjectRef<'_>,
    allow: &[String],
    allow_derived: &[String],
    sink_writer: W,
    err: &mut dyn Write,
) -> Result<StoredObjectExportOutcome, ExitCode> {
    let mut mounted = mount_tape_object(report, target, allow, allow_derived, err)?;
    let (bytes_written, object_sha256) = {
        let mut source = DriveHandleSource(&mut mounted.drive);
        if let Err(error) = source.space(
            i64::from(mounted.loc.tape_file_number),
            SpaceKind::Filemarks,
        ) {
            let _ = writeln!(err, "error: space to object tape file: {error}");
            return Err(ExitCode::from(1));
        }
        copy_stored_object_blocks(
            &mut source,
            mounted.plan.block_size_bytes as usize,
            mounted.plan.block_count,
            sink_writer,
        )
        .map_err(|error| {
            let _ = writeln!(err, "error: export object: {error}");
            ExitCode::from(1)
        })?
    };

    Ok(StoredObjectExportOutcome {
        object_id: mounted.loc.object_id,
        block_count: mounted.plan.block_count,
        block_size_bytes: mounted.plan.block_size_bytes,
        representation: mounted.plan.representation,
        bytes_written,
        object_sha256,
    })
}

fn copy_stored_object_blocks<W: Write>(
    source: &mut dyn BlockSource,
    block_size: usize,
    block_count: u64,
    mut writer: W,
) -> Result<(u64, [u8; 32]), String> {
    if block_size == 0 {
        return Err("block size must be nonzero".to_string());
    }

    let mut block = vec![0u8; block_size];
    let mut bytes_written = 0u64;
    let mut hasher = sha2::Sha256::new();
    for block_index in 0..block_count {
        let read = source
            .read_block(&mut block)
            .map_err(|error| format!("read object block {block_index}: {error}"))?;
        if read != block_size {
            return Err(format!(
                "short object block {block_index}: expected {block_size} bytes, got {read}"
            ));
        }
        writer
            .write_all(&block)
            .map_err(|error| format!("write object block {block_index}: {error}"))?;
        hasher.update(&block);
        bytes_written = bytes_written
            .checked_add(read as u64)
            .ok_or_else(|| "object byte count overflow".to_string())?;
    }
    writer
        .flush()
        .map_err(|error| format!("flush object output: {error}"))?;
    Ok((bytes_written, hasher.finalize().into()))
}

fn write_single_payload_from_read_object<W: Write>(
    object: &RemTarReadObject,
    mut writer: W,
) -> Result<(u64, [u8; 32]), String> {
    let mut payload_entries = 0u32;
    let mut payload_bytes = 0u64;
    let mut hasher = sha2::Sha256::new();
    for entry in &object.entries {
        if entry.path == MANIFEST_PATH {
            continue;
        }
        payload_entries = payload_entries.saturating_add(1);
        payload_bytes = payload_bytes
            .checked_add(entry.data.len() as u64)
            .ok_or_else(|| "payload byte count overflow".to_string())?;
        hasher.update(&entry.data);
        writer
            .write_all(&entry.data)
            .map_err(|error| format!("write payload: {error}"))?;
    }
    if payload_entries == 0 {
        return Err("object contains no payload entry".to_string());
    }
    if payload_entries > 1 {
        return Err(format!(
            "object contains {payload_entries} payload entries; single-file restore only (no --path in v1)"
        ));
    }
    writer
        .flush()
        .map_err(|error| format!("flush payload: {error}"))?;
    let digest: [u8; 32] = hasher.finalize().into();
    Ok((payload_bytes, digest))
}

/// Pure: build the verify receipt from expected vs streamed hash.
fn build_verify_receipt(expected: [u8; 32], actual: [u8; 32]) -> ArchiveVerifyReceipt {
    ArchiveVerifyReceipt {
        verified: expected == actual,
        expected_sha256: bytes_to_hex(&expected),
        actual_sha256: bytes_to_hex(&actual),
    }
}

/// Run `rem-debug archive read`: locator -> restored payload at `--out`.
pub fn run_archive_read(
    report: &remanence_library::DiscoveryReport,
    args: &ArchiveReadArgs,
    allow: &[String],
    allow_derived: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let (temp_out, out_file) = match create_archive_read_temp_output(&args.out) {
        Ok(pair) => pair,
        Err(e) => {
            let _ = writeln!(
                err,
                "error: create temporary --out {}: {e}",
                args.out.display()
            );
            return ExitCode::from(1);
        }
    };

    let target = TapeObjectRef {
        library: &args.library,
        config: &args.config,
        locator_json: &args.locator,
        key_file: args.key_file.as_deref(),
        private_key: args.private_key.as_deref(),
    };
    let outcome = match stream_tape_object(report, &target, allow, allow_derived, out_file, err) {
        Ok(o) => o,
        Err(code) => {
            let _ = std::fs::remove_file(&temp_out);
            return code;
        }
    };

    let verified = outcome.actual_sha256 == outcome.locator_content_sha256;
    if !verified {
        let _ = std::fs::remove_file(&temp_out);
    } else if let Err(e) = std::fs::rename(&temp_out, &args.out) {
        let _ = std::fs::remove_file(&temp_out);
        let _ = writeln!(err, "error: install --out {}: {e}", args.out.display());
        return ExitCode::from(1);
    }

    let receipt = ArchiveReadReceipt {
        object_id: outcome.object_id.clone(),
        bytes_written: outcome.payload_bytes,
        content_sha256: bytes_to_hex(&outcome.actual_sha256),
        verified,
    };
    let line = serde_json::to_string(&receipt).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
    let _ = writeln!(out, "{line}");

    if verified {
        ExitCode::SUCCESS
    } else {
        let _ = writeln!(
            err,
            "error: content_sha256 mismatch (tape payload vs locator)"
        );
        ExitCode::from(1)
    }
}

/// Run `rem-debug archive export-object`: locator -> complete stored object at `--out`.
pub fn run_archive_export_object(
    report: &remanence_library::DiscoveryReport,
    args: &ArchiveExportObjectArgs,
    allow: &[String],
    allow_derived: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let (temp_out, out_file) = match create_archive_read_temp_output(&args.out) {
        Ok(pair) => pair,
        Err(e) => {
            let _ = writeln!(
                err,
                "error: create temporary --out {}: {e}",
                args.out.display()
            );
            return ExitCode::from(1);
        }
    };

    let target = TapeObjectRef {
        library: &args.library,
        config: &args.config,
        locator_json: &args.locator,
        key_file: None,
        private_key: None,
    };
    let outcome =
        match stream_stored_tape_object(report, &target, allow, allow_derived, out_file, err) {
            Ok(o) => o,
            Err(code) => {
                let _ = std::fs::remove_file(&temp_out);
                return code;
            }
        };

    if let Err(e) = std::fs::rename(&temp_out, &args.out) {
        let _ = std::fs::remove_file(&temp_out);
        let _ = writeln!(err, "error: install --out {}: {e}", args.out.display());
        return ExitCode::from(1);
    }

    let receipt = ArchiveExportObjectReceipt {
        object_id: outcome.object_id,
        bytes_written: outcome.bytes_written,
        block_count: outcome.block_count,
        block_size_bytes: outcome.block_size_bytes,
        representation: outcome.representation,
        object_sha256: bytes_to_hex(&outcome.object_sha256),
    };
    let line = serde_json::to_string(&receipt).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
    let _ = writeln!(out, "{line}");

    ExitCode::SUCCESS
}

fn create_archive_read_temp_output(out: &Path) -> io::Result<(PathBuf, std::fs::File)> {
    let parent = out.parent().unwrap_or_else(|| Path::new("."));
    let base = out
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "archive-read".into());
    let pid = std::process::id();
    for attempt in 0..100u32 {
        let temp = parent.join(format!(".{base}.rem-read-{pid}-{attempt}.tmp"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
        {
            Ok(file) => return Ok((temp, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate unique temporary output path",
    ))
}

/// Run `rem-debug archive verify`: stream the object, hash the payload, and
/// compare to `--expected-sha256`.
pub fn run_archive_verify(
    report: &remanence_library::DiscoveryReport,
    args: &ArchiveVerifyArgs,
    allow: &[String],
    allow_derived: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let expected: [u8; 32] = match hex_to_bytes(&args.expected_sha256).and_then(|v| {
        <[u8; 32]>::try_from(v.as_slice())
            .map_err(|_| format!("expected-sha256 must be 32 bytes, got {}", v.len()))
    }) {
        Ok(e) => e,
        Err(e) => {
            let _ = writeln!(err, "error: --expected-sha256: {e}");
            return ExitCode::from(1);
        }
    };

    let target = TapeObjectRef {
        library: &args.library,
        config: &args.config,
        locator_json: &args.locator,
        key_file: args.key_file.as_deref(),
        private_key: args.private_key.as_deref(),
    };
    let outcome =
        match stream_tape_object(report, &target, allow, allow_derived, std::io::sink(), err) {
            Ok(o) => o,
            Err(code) => return code,
        };

    let receipt = build_verify_receipt(expected, outcome.actual_sha256);
    let verified = receipt.verified;
    let line = serde_json::to_string(&receipt).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
    let _ = writeln!(out, "{line}");

    if verified {
        ExitCode::SUCCESS
    } else {
        let _ = writeln!(
            err,
            "error: sha256 mismatch (tape payload vs --expected-sha256)"
        );
        ExitCode::from(1)
    }
}

/// Arguments for `rem-debug archive list`.
pub struct ArchiveListArgs {
    /// Path to config file.
    pub config: PathBuf,
}

/// One catalog record emitted by `archive list` — one JSON line per object copy.
///
/// Shape is the canonical `archive write --json` locator (so a consumer can
/// reconstruct the same locator key) plus `size_bytes` and the copy `status`.
/// `content_sha256` doubles as the integrity hash in this content-addressed
/// model. This is a read over rem's catalog projection; it never touches tape.
#[derive(serde::Serialize)]
struct CatalogObjectRecord {
    tape_uuid: String,
    tape_file_number: u32,
    first_body_lba: u64,
    object_id: String,
    caller_object_id: Option<String>,
    content_sha256: String,
    pool_id: Option<String>,
    body_format: Option<String>,
    size_bytes: Option<u64>,
    status: String,
}

/// Run `rem-debug archive list`: enumerate native objects from the local
/// catalog projection (no tape access) as one JSON line per object copy.
///
/// Backs Sutradhara's reconciliation scrub: it reports what *rem's catalog*
/// holds for this library. Keeping rem's catalog faithful to the physical
/// tape is a separate, rem-internal concern, not this command's job.
pub fn run_archive_list(
    args: &ArchiveListArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let mut state_handle = match StateHandle::open_from_config_file(&args.config) {
        Ok(h) => h,
        Err(e) => {
            let _ = writeln!(err, "error: open state: {e}");
            return ExitCode::from(1);
        }
    };

    let objects = match state_handle.catalog_index().list_native_objects() {
        Ok(objs) => objs,
        Err(e) => {
            let _ = writeln!(err, "error: list native objects: {e}");
            return ExitCode::from(1);
        }
    };

    let mut skipped_no_hash: u64 = 0;
    for obj in &objects {
        let content_sha256 = match &obj.content_hash {
            Some(h) => bytes_to_hex(h),
            None => {
                // No content hash -> no reconciliation identity to report.
                skipped_no_hash += 1;
                continue;
            }
        };
        for copy in &obj.copies {
            let record = CatalogObjectRecord {
                tape_uuid: bytes_to_hex(&copy.tape_uuid),
                tape_file_number: copy.tape_file_number,
                first_body_lba: copy.first_body_lba,
                object_id: obj.object_id.clone(),
                caller_object_id: obj.caller_object_id.clone(),
                content_sha256: content_sha256.clone(),
                pool_id: copy.pool_id.clone(),
                body_format: Some(obj.body_format.clone()),
                size_bytes: obj.logical_size_bytes,
                status: copy.status.clone(),
            };
            let line =
                serde_json::to_string(&record).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
            let _ = writeln!(out, "{line}");
        }
    }

    if skipped_no_hash > 0 {
        let _ = writeln!(
            err,
            "warning: skipped {skipped_no_hash} object(s) with no content hash"
        );
    }
    ExitCode::SUCCESS
}

// =====================================================================
//  Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use remanence_api::{PoolWriteObjectCopyRecord, PoolWriteObjectRecord};
    use remanence_library::model::{DriveBay, ElementLayout, IdentitySource, InstalledDrive};
    use remanence_library::{LoadError, LoadPlan, VecBlockSource};
    use remanence_state::{
        NativeObjectCopyRecord, TapeFileRecord, OBJECT_COPY_REPRESENTATION_PLAINTEXT,
    };
    use sha2::Digest;
    use uuid::Uuid;

    use crate::bytes_to_hex;

    // ---- bytes_to_hex ----

    #[test]
    fn bytes_to_hex_empty() {
        assert_eq!(bytes_to_hex(&[]), "");
    }

    #[test]
    fn bytes_to_hex_known() {
        assert_eq!(bytes_to_hex(&[0x00, 0xff, 0xab]), "00ffab");
    }

    #[test]
    fn build_verify_receipt_matches_and_mismatches() {
        let a = [0xABu8; 32];
        let b = [0xCDu8; 32];

        let ok = super::build_verify_receipt(a, a);
        assert!(ok.verified);
        assert_eq!(ok.expected_sha256, ok.actual_sha256);
        assert_eq!(ok.expected_sha256, bytes_to_hex(&a));

        let bad = super::build_verify_receipt(a, b);
        assert!(!bad.verified);
        assert_eq!(bad.expected_sha256, bytes_to_hex(&a));
        assert_eq!(bad.actual_sha256, bytes_to_hex(&b));
    }

    #[test]
    fn hex_to_bytes_decodes_and_rejects() {
        assert_eq!(
            super::hex_to_bytes("00ffab").unwrap(),
            vec![0x00, 0xff, 0xab]
        );
        assert_eq!(super::hex_to_bytes("").unwrap(), Vec::<u8>::new());
        assert!(super::hex_to_bytes("abc").is_err());
        assert!(super::hex_to_bytes("zz").is_err());
    }

    #[test]
    fn archive_read_temp_output_does_not_truncate_existing_destination() {
        use std::io::Write as _;

        let dir = tempfile::Builder::new()
            .prefix("remanence-cli-archive-read")
            .tempdir()
            .unwrap();
        let dest = dir.path().join("out.bin");
        std::fs::write(&dest, b"keep").unwrap();

        let (temp, mut file) = super::create_archive_read_temp_output(&dest).unwrap();
        file.write_all(b"new").unwrap();
        drop(file);

        assert_eq!(std::fs::read(&dest).unwrap(), b"keep");
        assert_ne!(temp, dest);
        std::fs::remove_file(temp).unwrap();
    }

    #[test]
    fn copy_stored_object_blocks_copies_exact_fixed_blocks() {
        let blocks = vec![vec![1u8, 2, 3, 4], vec![5u8, 6, 7, 8]];
        let expected_bytes = blocks.concat();
        let mut source = VecBlockSource::new(blocks);
        let mut out = Vec::new();

        let (bytes_written, digest) =
            super::copy_stored_object_blocks(&mut source, 4, 2, &mut out).expect("copy");

        assert_eq!(bytes_written, 8);
        assert_eq!(out, expected_bytes);
        let expected_digest: [u8; 32] = sha2::Sha256::digest(&expected_bytes).into();
        assert_eq!(digest, expected_digest);
    }

    #[test]
    fn copy_stored_object_blocks_rejects_short_blocks() {
        let mut source = VecBlockSource::new(vec![vec![1u8, 2, 3]]);
        let mut out = Vec::new();

        let err = super::copy_stored_object_blocks(&mut source, 4, 1, &mut out)
            .expect_err("short block rejected");

        assert!(err.contains("short object block 0"));
        assert!(out.is_empty());
    }

    #[test]
    fn decode_locator_parses_and_validates() {
        let raw = r#"{
            "tape_uuid":"000102030405060708090a0b0c0d0e0f",
            "tape_file_number":1,
            "first_body_lba":1,
            "object_id":"11111111-1111-1111-1111-111111111111",
            "caller_object_id":"c-1",
            "content_sha256":"0000000000000000000000000000000000000000000000000000000000000000",
            "pool_id":"scenario-a",
            "body_format":"rao-v1"
        }"#;
        let loc = super::decode_locator(raw).expect("decode");
        assert_eq!(
            loc.tape_uuid,
            [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
        );
        assert_eq!(loc.tape_file_number, 1);
        assert_eq!(loc.first_body_lba, 1);
        assert_eq!(loc.object_id, "11111111-1111-1111-1111-111111111111");
        assert_eq!(loc.content_sha256, [0u8; 32]);

        let bad = raw.replace("000102030405060708090a0b0c0d0e0f", "00ff");
        assert!(super::decode_locator(&bad).is_err());
        assert!(super::decode_locator("{ not json").is_err());
    }

    fn decoded(
        object_id: &str,
        tape_file_number: u32,
        first_body_lba: u64,
    ) -> super::DecodedLocator {
        super::DecodedLocator {
            tape_uuid: [7u8; 16],
            object_id: object_id.to_string(),
            tape_file_number,
            first_body_lba,
            content_sha256: [0u8; 32],
        }
    }

    fn copy(object_id: &str, tape_file_number: u32, first_body_lba: u64) -> NativeObjectCopyRecord {
        NativeObjectCopyRecord {
            object_id: object_id.to_string(),
            tape_uuid: vec![7u8; 16],
            tape_file_number,
            first_body_lba,
            first_parity_data_ordinal: None,
            protected_until_ordinal: None,
            status: "committed".to_string(),
            pool_id: Some("scenario-a".to_string()),
            representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT.to_string(),
            recipient_epoch_ids: None,
            metadata_frame_len: None,
            plaintext_digest: Some(vec![0x51; 32]),
            stored_digest: Some(vec![0x51; 32]),
        }
    }

    fn obj_tape_file(object_id: &str, tape_file_number: u32, block_count: u64) -> TapeFileRecord {
        TapeFileRecord {
            tape_uuid: vec![7u8; 16],
            tape_file_number,
            kind: "object".to_string(),
            block_count,
            object_id: Some(object_id.to_string()),
        }
    }

    #[test]
    fn plan_from_records_resolves_block_count_and_size() {
        let loc = decoded("obj-1", 3, 16);
        let plan = super::plan_from_records(
            &[copy("obj-1", 3, 16)],
            &[obj_tape_file("obj-1", 3, 7)],
            Some(65536),
            Some([0xAA; 32]),
            &loc,
        )
        .expect("plan");
        assert_eq!(plan.block_count, 7);
        assert_eq!(plan.block_size_bytes, 65536);
        assert_eq!(plan.manifest_sha256, Some([0xAA; 32]));
    }

    #[test]
    fn plan_from_records_rejects_missing_copy_file_and_size() {
        let loc = decoded("obj-1", 3, 16);
        assert!(super::plan_from_records(
            &[],
            &[obj_tape_file("obj-1", 3, 7)],
            Some(65536),
            None,
            &loc,
        )
        .is_err());
        assert!(
            super::plan_from_records(&[copy("obj-1", 3, 16)], &[], Some(65536), None, &loc,)
                .is_err()
        );
        assert!(super::plan_from_records(
            &[copy("obj-1", 3, 16)],
            &[obj_tape_file("obj-1", 3, 7)],
            None,
            None,
            &loc,
        )
        .is_err());
    }

    // ---- resolve_load_target (load-by-UUID bridge logic) ----

    fn test_inquiry() -> remanence_library::scsi::Inquiry {
        remanence_library::scsi::Inquiry {
            device_type: remanence_library::scsi::DeviceType::MediumChanger,
            peripheral_qualifier: 0,
            removable: true,
            version: 7,
            response_data_format: 2,
            additional_length: 31,
            vendor: *b"TEST    ",
            product: *b"LIBRARY         ",
            revision: *b"0001",
        }
    }

    fn make_test_library(
        drive_bays: Vec<DriveBay>,
        slots: Vec<remanence_library::Slot>,
    ) -> remanence_library::Library {
        use std::path::PathBuf;
        remanence_library::Library {
            serial: "TEST_LIB".to_string(),
            changer_sg: PathBuf::from("/dev/sg0"),
            changer_sysfs: PathBuf::from("/sys/test"),
            changer_inquiry: test_inquiry(),
            chassis_designator: None,
            layout: ElementLayout {
                robot_address: 0,
                drive_start: 0x0100,
                drive_count: drive_bays.len() as u16,
                slot_start: 0x0400,
                slot_count: slots.len() as u16,
                ie_start: 0x0200,
                ie_count: 0,
            },
            drive_bays,
            slots,
            ie_ports: vec![],
        }
    }

    fn make_empty_bay(addr: u16) -> DriveBay {
        DriveBay {
            element_address: addr,
            accessible: true,
            exception: None,
            installed: Some(InstalledDrive {
                serial: "DRV_UNIT".to_string(),
                identity_source: IdentitySource::DvcidAndInquiry,
                vendor: Some("TEST".to_string()),
                product: Some("DRIVE".to_string()),
                revision: Some("0001".to_string()),
                sg_path: Some(PathBuf::from(format!("/dev/sg-{addr:04x}"))),
                sysfs_path: Some(PathBuf::from(format!("/sys/test/{addr:04x}"))),
            }),
            loaded: false,
            loaded_tape: None,
            source_slot: None,
        }
    }

    fn make_loaded_bay(addr: u16, voltag: &str) -> DriveBay {
        DriveBay {
            element_address: addr,
            accessible: true,
            exception: None,
            installed: Some(InstalledDrive {
                serial: "DRV_UNIT".to_string(),
                identity_source: IdentitySource::DvcidInline,
                vendor: None,
                product: None,
                revision: None,
                sg_path: None,
                sysfs_path: None,
            }),
            loaded: true,
            loaded_tape: Some(voltag.to_string()),
            source_slot: None,
        }
    }

    #[test]
    fn resolve_load_target_voltag_in_slot() {
        let lib = make_test_library(
            vec![make_empty_bay(0x0100)],
            vec![remanence_library::Slot {
                element_address: 0x0400,
                accessible: true,
                exception: None,
                full: true,
                cartridge: Some("RMN001L9".to_string()),
            }],
        );
        let plan = remanence_library::resolve_load_target(&lib, "RMN001L9").unwrap();
        assert_eq!(
            plan,
            LoadPlan::Load {
                slot: 0x0400,
                bay: 0x0100
            }
        );
    }

    #[test]
    fn resolve_load_target_voltag_already_loaded() {
        let lib = make_test_library(vec![make_loaded_bay(0x0100, "RMN001L9")], vec![]);
        let plan = remanence_library::resolve_load_target(&lib, "RMN001L9").unwrap();
        assert_eq!(plan, LoadPlan::AlreadyLoaded { bay: 0x0100 });
    }

    #[test]
    fn resolve_load_target_voltag_not_found() {
        let lib = make_test_library(
            vec![make_empty_bay(0x0100)],
            vec![remanence_library::Slot {
                element_address: 0x0400,
                accessible: true,
                exception: None,
                full: true,
                cartridge: Some("OTHER001L9".to_string()),
            }],
        );
        let err = remanence_library::resolve_load_target(&lib, "RMN001L9").unwrap_err();
        assert!(matches!(err, LoadError::NotInLibrary));
    }

    #[test]
    fn resolve_load_target_no_free_drive() {
        let lib = make_test_library(
            vec![make_loaded_bay(0x0100, "OTHER001L9")],
            vec![remanence_library::Slot {
                element_address: 0x0400,
                accessible: true,
                exception: None,
                full: true,
                cartridge: Some("RMN001L9".to_string()),
            }],
        );
        let err = remanence_library::resolve_load_target(&lib, "RMN001L9").unwrap_err();
        assert!(matches!(err, LoadError::NoFreeDrive));
    }

    // ---- JSON locator shape (§4 contract pin) -------------------------

    #[test]
    fn json_locator_has_all_required_fields() {
        let tape_uuid_bytes = *Uuid::new_v4().as_bytes();
        let object_id_bytes = *Uuid::new_v4().as_bytes();
        let sha256 = [0xabu8; 32];

        let object = PoolWriteObjectRecord {
            object_id: object_id_bytes,
            caller_object_id: "caller-123".to_string(),
            content_sha256: sha256,
            logical_size_bytes: 12345,
            body_format: "rao-v1".to_string(),
            created_at_utc: "2026-05-29T00:00:00Z".to_string(),
            copies: vec![PoolWriteObjectCopyRecord {
                tape_uuid: tape_uuid_bytes,
                tape_file_number: 1,
                first_body_lba: 2,
                pool_id: "scenario-a".to_string(),
                representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT.to_string(),
                recipient_epoch_ids: None,
                metadata_frame_len: None,
            }],
        };

        let mut out = Vec::<u8>::new();
        super::print_locator_json(&object, "scenario-a", &mut out);
        let line = String::from_utf8(out).unwrap();
        let line = line.trim();

        // Must not be empty.
        assert!(!line.is_empty());
        // Must parse as valid JSON.
        let parsed: serde_json::Value = serde_json::from_str(line).expect("valid JSON");

        // Every §4 field must be present.
        for field in &[
            "tape_uuid",
            "tape_file_number",
            "first_body_lba",
            "object_id",
            "caller_object_id",
            "content_sha256",
            "pool_id",
            "body_format",
            "representation",
            "encryption",
            "format_version",
            "recipient_epochs",
            "metadata_frame_len",
            "append_commit_info",
        ] {
            assert!(parsed.get(field).is_some(), "missing field: {field}");
        }

        // Check values.
        assert_eq!(parsed["caller_object_id"].as_str().unwrap(), "caller-123");
        assert_eq!(parsed["tape_file_number"].as_u64().unwrap(), 1);
        assert_eq!(parsed["first_body_lba"].as_u64().unwrap(), 2);
        assert_eq!(parsed["pool_id"].as_str().unwrap(), "scenario-a");
        assert_eq!(parsed["body_format"].as_str().unwrap(), "rao-v1");
        assert_eq!(parsed["representation"].as_str().unwrap(), "plaintext");
        assert_eq!(parsed["encryption"].as_str().unwrap(), "none");
        assert!(parsed["format_version"].is_null());
        assert!(parsed["recipient_epochs"].is_null());
        assert!(parsed["metadata_frame_len"].is_null());
        let append_info = parsed["append_commit_info"].as_object().unwrap();
        assert_eq!(append_info["append_mode"].as_str().unwrap(), "fresh");
        assert_eq!(append_info["tape_file_number"].as_u64().unwrap(), 1);
        assert_eq!(append_info["first_body_lba"].as_u64().unwrap(), 2);
        assert!(append_info["position_before_lba"].is_null());
        assert!(append_info["position_after_lba"].is_null());
        assert!(append_info["journal_record_ordinal"].is_null());
        assert!(append_info["estimated_remaining_bytes"].is_null());
        assert!(append_info["sealed_after_write"].is_null());

        // tape_uuid must be 32 lowercase hex chars.
        let tape_uuid_str = parsed["tape_uuid"].as_str().unwrap();
        assert_eq!(tape_uuid_str.len(), 32, "tape_uuid must be 32 hex chars");
        assert!(
            tape_uuid_str.chars().all(|c| c.is_ascii_hexdigit()),
            "tape_uuid must be lowercase hex: {tape_uuid_str}"
        );

        // content_sha256 must be 64 lowercase hex chars.
        let sha_str = parsed["content_sha256"].as_str().unwrap();
        assert_eq!(sha_str.len(), 64, "content_sha256 must be 64 hex chars");
        assert_eq!(&sha_str[..2], "ab", "first byte of sha256 mismatch");

        // object_id must be a valid UUID string.
        Uuid::parse_str(parsed["object_id"].as_str().unwrap())
            .expect("object_id must be a valid UUID string");
    }
}
