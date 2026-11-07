//! Declared authority reset. Scope comes from the current typed log, never SQL.
//! A durable archive receipt precedes deletion and pins the replacement identity;
//! retries retain the startup fence until manifest, boundary and projection sync.
use super::*;
use crate::{
    authority_manifest::AuthorityManifest, library_log as lib, library_runtime::LibraryRuntime,
};
use serde::{Deserialize, Serialize};
use std::{io::Read, sync::Arc};

const RECEIPT: &str = "authority-reset-receipt.json";
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u32,
    request_digest: String,
    preflight_token: String,
    config_digest: String,
    allow_erase: Vec<String>,
    archive: PathBuf,
    archive_digest: String,
    old_library_uuid: Option<[u8; 16]>,
    old_terminal_hash: Option<[u8; 32]>,
    new_library_uuid: [u8; 16],
    tapes: Vec<CatalogResetPreflightTape>,
    phase: Phase,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Phase {
    Archived,
    Cleared,
    Manifest,
    Boundary,
    Complete,
}
fn refuse(message: impl Into<String>) -> StateError {
    StateError::ConfigInvalid(message.into())
}
fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}
fn digest(bytes: &[u8]) -> String {
    hex(Sha256::digest(bytes))
}

/// Canonicalize existing ancestors without creating a directory or following a
/// symlink. The result also binds paths whose final component does not exist.
fn resolved(path: &Path) -> Result<PathBuf, StateError> {
    crate::authority_manifest::no_symlinks(path)?;
    let absolute =
        std::path::absolute(path).map_err(|e| StateError::io_at("resolve reset path", path, e))?;
    let mut existing = absolute.as_path();
    let mut tail = Vec::new();
    while !existing.exists() {
        tail.push(
            existing
                .file_name()
                .ok_or_else(|| refuse("invalid reset path"))?
                .to_os_string(),
        );
        existing = existing
            .parent()
            .ok_or_else(|| refuse("reset path has no existing ancestor"))?;
    }
    let mut result = fs::canonicalize(existing)
        .map_err(|e| StateError::io_at("canonicalize reset ancestor", existing, e))?;
    for part in tail.into_iter().rev() {
        result.push(part);
    }
    Ok(result)
}
fn inspect_tree(path: &Path) -> Result<(), StateError> {
    crate::authority_manifest::no_symlinks(path)?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(StateError::io_at("inspect reset source", path, e)),
    };
    if metadata.is_dir() {
        for entry in
            fs::read_dir(path).map_err(|e| StateError::io_at("read reset source", path, e))?
        {
            inspect_tree(
                &entry
                    .map_err(|e| StateError::io_at("read reset entry", path, e))?
                    .path(),
            )?;
        }
    } else if !metadata.is_file() {
        return Err(refuse(format!("non-file reset source: {}", path.display())));
    }
    Ok(())
}
fn paths_preflight(paths: &StatePaths) -> Result<(), StateError> {
    let targets = vec![
        paths.audit_dir.clone(),
        paths.journal_dir.clone(),
        paths.tape_cache_dir.clone(),
        paths.sqlite_path.clone(),
        PathBuf::from(format!("{}-wal", paths.sqlite_path.display())),
        PathBuf::from(format!("{}-shm", paths.sqlite_path.display())),
        paths.state_dir.join("authority-manifest.json"),
        paths.state_dir.join("authority-format"),
        paths.state_dir.join(RECEIPT),
        catalog_reset_fence_path(paths),
        paths
            .state_dir
            .join(format!("{CATALOG_RESET_FENCE_FILE}.new")),
        paths.state_dir.join("state.lock"),
    ];
    let canonical = targets
        .iter()
        .map(|p| resolved(p))
        .collect::<Result<Vec<_>, _>>()?;
    for (i, a) in canonical.iter().enumerate() {
        for b in &canonical[i + 1..] {
            if a.starts_with(b) || b.starts_with(a) {
                return Err(refuse("reset deletion paths overlap"));
            }
        }
        for preserved in [
            &paths.config_path,
            &paths.calibration_dir,
            &paths.state_dir.join(RESET_ARCHIVE_DIR),
        ] {
            let b = resolved(preserved)?;
            if b.starts_with(a) || a.starts_with(&b) {
                return Err(refuse(
                    "reset deletion overlaps config, calibration or archive",
                ));
            }
        }
        if resolved(&paths.state_dir)?.starts_with(a) {
            return Err(refuse("reset target contains the state lease"));
        }
    }
    for target in targets {
        inspect_tree(&target)?;
    }
    // Reset appends calibration eviction authority; inspect that preserved
    // namespace before creating a fence or writing lock diagnostics too.
    inspect_tree(&paths.calibration_dir)?;
    for suffix in ["-wal", "-shm"] {
        inspect_tree(&PathBuf::from(format!(
            "{}{suffix}",
            paths.sqlite_path.display()
        )))?;
    }
    inspect_tree(&paths.state_dir.join("authority-format"))?;
    for path in [
        &paths.state_dir.join(RECEIPT),
        &paths.state_dir.join("authority-manifest.json"),
        &catalog_reset_fence_path(paths),
    ] {
        inspect_tree(path)?;
    }
    Ok(())
}
fn hash_tree(hash: &mut Sha256, root: &Path, relative: &Path) -> Result<(), StateError> {
    let path = if relative.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(relative)
    };
    let m = fs::symlink_metadata(&path)
        .map_err(|e| StateError::io_at("hash reset source", &path, e))?;
    hash_catalog_reset_field(hash, relative.as_os_str().as_encoded_bytes());
    if m.is_dir() {
        hash.update(b"directory\0");
        let mut entries = fs::read_dir(&path)
            .map_err(|e| StateError::io_at("read reset hash directory", &path, e))?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| StateError::io_at("read reset hash entry", &path, e))?;
        entries.sort();
        for name in entries {
            hash_tree(hash, root, &relative.join(name))?;
        }
    } else if m.is_file() {
        hash.update(b"file\0");
        hash.update(m.len().to_be_bytes());
        let mut file = fs::File::open(&path)
            .map_err(|e| StateError::io_at("read reset hash file", &path, e))?;
        let mut buffer = [0u8; 65536];
        loop {
            let n = file
                .read(&mut buffer)
                .map_err(|e| StateError::io_at("hash reset file", &path, e))?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
        }
    } else {
        return Err(refuse("reset hash encountered non-file authority"));
    }
    Ok(())
}
fn tree_digest(path: &Path) -> Result<String, StateError> {
    let mut hash = Sha256::new();
    hash_tree(&mut hash, path, Path::new(""))?;
    Ok(hex(hash.finalize()))
}
fn source_digest(paths: &StatePaths) -> Result<String, StateError> {
    let mut hash = Sha256::new();
    for (name, path) in [
        ("library", paths.audit_dir.clone()),
        ("native", paths.journal_dir.clone()),
        ("manifest", paths.state_dir.join("authority-manifest.json")),
        ("format", paths.state_dir.join("authority-format")),
    ] {
        hash_catalog_reset_field(&mut hash, name.as_bytes());
        hash_catalog_reset_field(&mut hash, resolved(&path)?.as_os_str().as_encoded_bytes());
        if path.exists() {
            hash_tree(&mut hash, &path, Path::new(""))?;
        } else {
            hash.update(b"absent\0");
        }
    }
    Ok(hex(hash.finalize()))
}

#[cfg(test)]
pub(super) fn test_authority_bytes(paths: &StatePaths) -> String {
    let mut hash = Sha256::new();
    for path in [
        &paths.audit_dir,
        &paths.journal_dir,
        &paths.state_dir.join("authority-manifest.json"),
    ] {
        hash_tree(&mut hash, path, Path::new("")).unwrap();
    }
    hex(hash.finalize())
}
fn load_receipt(paths: &StatePaths) -> Result<Option<Receipt>, StateError> {
    let path = paths.state_dir.join(RECEIPT);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| refuse(format!("invalid reset receipt: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(StateError::io_at("read reset receipt", path, e)),
    }
}
fn publish_receipt(paths: &StatePaths, receipt: &Receipt) -> Result<(), StateError> {
    publish_receipt_before_rename(paths, receipt, |_| Ok(()))
}

/// Keep the publication boundary explicit so fault tests can retain a synced,
/// unpublished temporary exactly where a process interruption would leave it.
fn publish_receipt_before_rename(
    paths: &StatePaths,
    receipt: &Receipt,
    before_rename: impl FnOnce(&mut crate::log_frame::publication::Temporary) -> Result<(), StateError>,
) -> Result<(), StateError> {
    let path = paths.state_dir.join(RECEIPT);
    let mut temporary =
        crate::log_frame::publication::Temporary::new(&paths.state_dir, "reset", "receipt")
            .map_err(|e| StateError::io_at("stage reset receipt", &paths.state_dir, e))?;
    let bytes = serde_json::to_vec_pretty(receipt).map_err(|e| refuse(e.to_string()))?;
    temporary
        .file
        .write_all(&bytes)
        .and_then(|_| temporary.file.sync_all())
        .map_err(|e| StateError::io_at("sync reset receipt", &temporary.path, e))?;
    before_rename(&mut temporary)?;
    remanence_library::durable_file::rename(&temporary.path, &path)
        .map_err(|e| StateError::io_at("publish reset receipt", &path, e))?;
    sync_directory(&paths.state_dir)
}

/// Scoped inventory plus the source library identity and terminal hash.
type SourceInventory = (
    Vec<CatalogResetPreflightTape>,
    Option<[u8; 16]>,
    Option<[u8; 32]>,
);

fn source(
    paths: &StatePaths,
    allow: &[String],
    whole: bool,
) -> Result<SourceInventory, StateError> {
    let manifest=AuthorityManifest::inspect_for_reset(paths).map_err(|e|refuse(format!("scoped reset cannot establish current typed inventory: {e}; the one-time old-binary preflight is a supervisor step, not an inventory importer")))?;
    let Some(manifest) = manifest else {
        return Ok((Vec::new(), None, None));
    };
    let view = lib::native_view::NativeTapeView::probe(manifest.tape_root(), &paths.sqlite_path)
        .map_err(|e| refuse(e.to_string()))?;
    let replay = lib::store::replay(manifest.library_root(), manifest.library_uuid(), &view)
        .map_err(|e| refuse(e.to_string()))?;
    if replay.refusal.is_some() || !replay.fold.pending.is_empty() {
        return Err(refuse(
            "reset inventory has unresolved or refused typed authority",
        ));
    }
    if !whole {
        let validated = view
            .snapshots()
            .map(|native| native.header().tape_uuid)
            .collect::<std::collections::BTreeSet<_>>();
        for path in crate::TapeAuthority::list_entries(manifest.tape_root())? {
            let uuid = crate::TapeAuthority::entry_tape_uuid(&path)?;
            if !validated.contains(&uuid) || !replay.fold.tapes.contains_key(&uuid) {
                return Err(refuse("scoped reset cannot establish label scope of native tape without validated native identity and Reprovision"));
            }
        }
    }
    let mut tapes = Vec::new();
    for value in replay.fold.tapes.values() {
        let label = value.label.as_ref().map(|v| v.as_str().to_string());
        if let Some(label) = &label {
            if !whole && !allow.contains(label) {
                return Err(refuse(format!(
                    "reset refuses tape {label:?} outside exact allow-erase list"
                )));
            }
        } else if !whole && !value.retired {
            return Err(refuse("reset refuses unlabelled live typed tape"));
        }
        // Preserve the original exact-scope rule: retired identities whose
        // label was cleared are exempt from the label selector. Retire is now
        // proved by the typed fold, never a SQL state string.
        tapes.push(CatalogResetPreflightTape {
            tape_uuid: value.tape.uuid,
            voltag: label,
            voltag_source: None,
            cartridge_generation: match value.medium.family {
                lib::MediumFamily::Lto(g) => Some(g.to_string()),
                lib::MediumFamily::Disk => None,
            },
            generation_source: None,
            kind: "data".into(),
            pool_id: value.pool.as_ref().map(|v| v.as_str().into()),
            assignment_generation: value.assignment_generation,
            state: if value.retired {
                "retired"
            } else {
                "typed_identity"
            }
            .into(),
            block_size: Some(u64::from(value.medium.record_size)),
            scheme_id: None,
            data_blocks_per_stripe: None,
            parity_blocks_per_stripe: None,
            stripes_per_neighborhood: None,
        });
    }
    for (uuid, value) in &replay.fold.cleaning_cartridges {
        if !whole && !allow.iter().any(|v| v == value.barcode.as_str()) {
            return Err(refuse(format!(
                "reset refuses cleaning cartridge {:?} outside exact allow-erase list",
                value.barcode.as_str()
            )));
        }
        tapes.push(CatalogResetPreflightTape {
            tape_uuid: *uuid,
            voltag: Some(value.barcode.as_str().into()),
            voltag_source: None,
            cartridge_generation: None,
            generation_source: None,
            kind: "cleaning".into(),
            pool_id: None,
            assignment_generation: 0,
            state: "registered".into(),
            block_size: None,
            scheme_id: None,
            data_blocks_per_stripe: None,
            parity_blocks_per_stripe: None,
            stripes_per_neighborhood: None,
        });
    }
    Ok((
        tapes,
        Some(manifest.library_uuid()),
        Some(replay.terminal_hash),
    ))
}

fn archive(paths: &StatePaths, mut receipt: Receipt) -> Result<Receipt, StateError> {
    let directory = create_unique_reset_archive_dir(&paths.state_dir)?;
    archive_directory_if_exists(&paths.audit_dir, &directory.join("audit"))?;
    archive_directory_if_exists(&paths.journal_dir, &directory.join("journals"))?;
    // SQL is copied as opaque forensic bytes, never opened or checkpointed.
    for (name, path) in [
        ("catalog.sqlite", paths.sqlite_path.clone()),
        (
            "catalog.sqlite-wal",
            PathBuf::from(format!("{}-wal", paths.sqlite_path.display())),
        ),
        (
            "catalog.sqlite-shm",
            PathBuf::from(format!("{}-shm", paths.sqlite_path.display())),
        ),
        (
            "authority-manifest.json",
            paths.state_dir.join("authority-manifest.json"),
        ),
        ("authority-format", paths.state_dir.join("authority-format")),
        ("config.snapshot", paths.config_path.clone()),
        ("previous-reset-receipt.json", paths.state_dir.join(RECEIPT)),
    ] {
        if path.exists() {
            let target = directory.join(name);
            fs::copy(&path, &target)
                .map_err(|e| StateError::io_at("archive reset input", &path, e))?;
            sync_regular_file(&target)?;
        }
    }
    sync_directory(&directory)?;
    sync_directory(directory.parent().unwrap())?;
    receipt.archive = resolved(&directory)?;
    receipt.archive_digest = tree_digest(&receipt.archive)?;
    publish_receipt(paths, &receipt)?;
    Ok(receipt)
}
/// Partial deletion is resumable, but every surviving authority byte must still
/// be represented by the durable archive. No new/changed source is discarded.
fn verify_survivors(active: &Path, archived: &Path) -> Result<(), StateError> {
    let metadata = match fs::symlink_metadata(active) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(StateError::io_at(
                "inspect surviving reset authority",
                active,
                error,
            ))
        }
    };
    let saved = match fs::symlink_metadata(archived) {
        Ok(saved) => saved,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && metadata.is_dir() => {
            let mut entries = fs::read_dir(active)
                .map_err(|e| StateError::io_at("inspect partial reset directory", active, e))?;
            if entries.next().is_none() {
                return Ok(());
            }
            return Err(refuse("unarchived authority appeared during reset"));
        }
        Err(error) => {
            return Err(StateError::io_at(
                "find archived surviving authority",
                archived,
                error,
            ))
        }
    };
    if metadata.is_dir() && saved.is_dir() {
        for entry in fs::read_dir(active)
            .map_err(|e| StateError::io_at("read surviving authority", active, e))?
        {
            let entry = entry
                .map_err(|e| StateError::io_at("read surviving authority entry", active, e))?;
            verify_survivors(&entry.path(), &archived.join(entry.file_name()))?;
        }
    } else if metadata.is_file() && saved.is_file() {
        if tree_digest(active)? != tree_digest(archived)? {
            return Err(refuse(format!(
                "surviving authority changed after archival: {}",
                active.display()
            )));
        }
    } else {
        return Err(refuse("surviving authority type differs from archive"));
    }
    Ok(())
}
fn verify_receipt(
    paths: &StatePaths,
    receipt: &Receipt,
    config_bytes: &[u8],
    allow: &[String],
    whole: bool,
) -> Result<(), StateError> {
    if receipt.version != 1
        || receipt.new_library_uuid == [0; 16]
        || receipt.old_library_uuid == Some(receipt.new_library_uuid)
        || receipt.config_digest != digest(config_bytes)
        || receipt.allow_erase != allow
    {
        return Err(refuse(
            "reset receipt differs from requested config or exact erase scope",
        ));
    }
    if resolved(
        receipt
            .archive
            .parent()
            .ok_or_else(|| refuse("reset archive lacks parent"))?,
    )? != resolved(&paths.state_dir.join(RESET_ARCHIVE_DIR))?
    {
        return Err(refuse(
            "reset receipt archive is outside the bound archive root",
        ));
    }
    if resolved(&receipt.archive)? != receipt.archive {
        return Err(refuse("reset archive receipt path is not canonical"));
    }
    inspect_tree(&receipt.archive)?;
    if tree_digest(&receipt.archive)? != receipt.archive_digest {
        return Err(refuse("reset archive digest differs from durable receipt"));
    }
    if receipt.phase == Phase::Archived {
        for (active, saved) in [
            (paths.audit_dir.clone(), receipt.archive.join("audit")),
            (paths.journal_dir.clone(), receipt.archive.join("journals")),
            (
                paths.state_dir.join("authority-manifest.json"),
                receipt.archive.join("authority-manifest.json"),
            ),
            (
                paths.state_dir.join("authority-format"),
                receipt.archive.join("authority-format"),
            ),
        ] {
            verify_survivors(&active, &saved)?;
        }
    }
    let request = catalog_reset_request_digest(
        paths,
        config_bytes,
        &[],
        allow,
        whole,
        &receipt.preflight_token,
    )?;
    if request != receipt.request_digest {
        return Err(refuse("reset receipt request binding differs"));
    }
    Ok(())
}
fn report(
    paths: &StatePaths,
    receipt: &Receipt,
    resume: bool,
) -> Result<CatalogResetPreflightReport, StateError> {
    let evidence = |path: &Path| -> Result<CatalogResetPathEvidence, StateError> {
        Ok(CatalogResetPathEvidence {
            configured_path: path.to_path_buf(),
            canonical_path: resolved(path)?,
            configured_path_is_symlink: false,
        })
    };
    Ok(CatalogResetPreflightReport {
        source_schema_version: 0,
        preflight_token: receipt.preflight_token.clone(),
        request_digest: receipt.request_digest.clone(),
        resume_exact: resume,
        paths: CatalogResetPreflightPaths {
            config: evidence(&paths.config_path)?,
            state_dir: evidence(&paths.state_dir)?,
            sqlite: evidence(&paths.sqlite_path)?,
            audit: evidence(&paths.audit_dir)?,
            journal: evidence(&paths.journal_dir)?,
            tape_cache: evidence(&paths.tape_cache_dir)?,
        },
        preserve_tape_voltags: Vec::new(),
        allow_erase_tape_voltags: receipt.allow_erase.clone(),
        tapes: receipt.tapes.clone(),
    })
}
fn prepared(
    paths: &StatePaths,
    config_bytes: &[u8],
    allow: &[String],
    whole: bool,
) -> Result<Receipt, StateError> {
    let (tapes, old_library_uuid, old_terminal_hash) = if whole {
        // Explicit whole-state reset carries no tape allowlist. Its one-time
        // old-binary preflight is performed by the supervisor (ruling9).
        if paths.state_dir.join("authority-manifest.json").exists() {
            source(paths, &[], true)?
        } else {
            (
                Vec::new(),
                None,
                crate::audit::FileAuditLog::archive_terminal_hash(&paths.audit_dir)?,
            )
        }
    } else {
        if FileAuditLog::archive_terminal_hash(&paths.audit_dir)?
            .is_some_and(|hash| hash != [0; 32])
        {
            return Err(refuse("scoped reset cannot infer legacy audit identities from current typed inventory; supervisor whole-state preflight is required"));
        }
        source(paths, allow, false)?
    };
    let source_token = source_digest(paths)?;
    let token =
        catalog_reset_request_digest(paths, config_bytes, &[], allow, whole, &source_token)?;
    let request = catalog_reset_request_digest(paths, config_bytes, &[], allow, whole, &token)?;
    Ok(Receipt {
        version: 1,
        request_digest: request,
        preflight_token: token,
        config_digest: digest(config_bytes),
        allow_erase: allow.to_vec(),
        archive: PathBuf::new(),
        archive_digest: String::new(),
        old_library_uuid,
        old_terminal_hash,
        new_library_uuid: *uuid::Uuid::new_v4().as_bytes(),
        tapes,
        phase: Phase::Archived,
    })
}

/// Scoped admission under a state lease; execution repeats the same checks.
pub(super) fn preflight(
    config_path: &Path,
    preserve: &[String],
    allow: &[String],
) -> Result<CatalogResetPreflightReport, StateError> {
    if !preserve.is_empty() {
        return Err(refuse("declared reset does not import preserved catalog identities; reinitialize or adopt physical media afterward"));
    }
    let mut allow = validate_catalog_reset_selectors(allow, "allow-erase")?;
    allow.sort();
    let config = load_config(config_path)?;
    let paths = StatePaths::from_config(config_path, &config);
    paths_preflight(&paths)?;
    let _lease = StateLockGuard::acquire(&paths.state_dir)?;
    let (locked, bytes) = load_catalog_reset_config_snapshot(config_path)?;
    ensure_catalog_reset_config_unchanged(&paths, &config, &locked)?;
    if let Some((active_request, _, _)) = catalog_reset_fence_evidence(&paths)? {
        let receipt = load_receipt(&paths)?
            .filter(|receipt| receipt.request_digest == active_request)
            .ok_or_else(|| {
                refuse("reset fence has no durable archive receipt yet; retry the exact reset")
            })?;
        verify_receipt(&paths, &receipt, &bytes, &allow, false)?;
        report(&paths, &receipt, true)
    } else {
        report(&paths, &prepared(&paths, &bytes, &allow, false)?, false)
    }
}

fn reset_diagnostic(receipt: &Receipt) -> lib::Diagnostic {
    let optional = |value: Option<Vec<u8>>| {
        value
            .map(|v| lib::DiagnosticValue::Bytes(lib::Blob(v)))
            .unwrap_or(lib::DiagnosticValue::Null(lib::Unit))
    };
    let identity = receipt
        .old_library_uuid
        .map(|v| format!("library:{}", hex(v)))
        .unwrap_or_else(|| format!("legacy-audit:{}", receipt.archive.join("audit").display()));
    lib::Diagnostic {
        event: lib::DiagnosticEvent::RecoveryEvent,
        subject_kind: "authority-reset".into(),
        subject_id: Some(receipt.request_digest.as_str().into()),
        detail: lib::Bounded(vec![
            lib::DiagnosticEntry(
                "archive_digest".into(),
                lib::DiagnosticValue::Text(receipt.archive_digest.as_str().into()),
            ),
            lib::DiagnosticEntry(
                "archive_path".into(),
                lib::DiagnosticValue::Text(receipt.archive.display().to_string().as_str().into()),
            ),
            lib::DiagnosticEntry(
                "archived_log_identity".into(),
                lib::DiagnosticValue::Text(identity.as_str().into()),
            ),
            lib::DiagnosticEntry(
                "old_library_uuid".into(),
                optional(receipt.old_library_uuid.map(|v| v.to_vec())),
            ),
            lib::DiagnosticEntry(
                "old_terminal_hash".into(),
                optional(receipt.old_terminal_hash.map(|v| v.to_vec())),
            ),
        ]),
    }
}
fn boundary(
    paths: &StatePaths,
    lease: Arc<StateLockGuard>,
    manifest: AuthorityManifest,
    receipt: &Receipt,
    config: &RemConfig,
) -> Result<(), StateError> {
    let view = lib::native_view::NativeTapeView::probe(manifest.tape_root(), &paths.sqlite_path)
        .map_err(|e| refuse(e.to_string()))?;
    let diagnostic = reset_diagnostic(receipt);
    let configuration = configuration_record(config)?;
    let mut existing = false;
    let replay = lib::store::replay_observed(
        manifest.library_root(),
        manifest.library_uuid(),
        &view,
        &mut |record| {
            match (record.sequence(), record.record().body_view()) {
                (1, lib::BodyView::Diagnostic(value)) if value == &diagnostic => existing = true,
                (2, lib::BodyView::ConfigurationAccepted(value)) if value == &configuration => (),
                _ => {
                    return Err(lib::store::StoreError::Fold(
                        lib::FoldError::WrongCounterpart,
                    ))
                }
            }
            Ok(())
        },
    )
    .map_err(|e| refuse(e.to_string()))?;
    if replay.refusal.is_some()
        || (!existing && receipt.phase != Phase::Manifest)
        || (receipt.phase == Phase::Complete && replay.last_sequence != 2)
    {
        return Err(refuse("new authority differs from reset receipt boundary"));
    }
    if existing {
        return Ok(());
    }
    let runtime = LibraryRuntime::open_bound_manifest(paths, true, lease, manifest)
        .map_err(|e| refuse(e.to_string()))?;
    runtime
        .append(|a, v| {
            a.diagnostic(
                diagnostic,
                lib::AuditActor::System,
                lib::SourceLayer::Layer4,
                v,
            )
        })
        .map_err(|e| refuse(e.to_string()))?;
    Ok(())
}

pub(super) fn execute(
    paths: StatePaths,
    config: RemConfig,
    preserve: &[String],
    allow: &[String],
    expected: Option<&str>,
    whole: bool,
) -> Result<CatalogResetReport, StateError> {
    execute_with_hook(paths, config, preserve, allow, expected, whole, |_| Ok(()))
}
fn execute_with_hook(
    paths: StatePaths,
    config: RemConfig,
    preserve: &[String],
    allow: &[String],
    expected: Option<&str>,
    whole: bool,
    mut hook: impl FnMut(Phase) -> Result<(), StateError>,
) -> Result<CatalogResetReport, StateError> {
    reset_helper_tripwire();
    if !preserve.is_empty() {
        return Err(refuse(
            "declared reset cannot restore SQL tape identities; reinitialize or adopt afterward",
        ));
    }
    let mut allow = validate_catalog_reset_selectors(allow, "allow-erase")?;
    allow.sort();
    paths_preflight(&paths)?;
    let lease = Arc::new(StateLockGuard::acquire(&paths.state_dir)?);
    crate::projector::ensure_reset_catalog_idle(&paths.sqlite_path)?;
    let (config, bytes) = {
        let (locked, bytes) = load_catalog_reset_config_snapshot(&paths.config_path)?;
        ensure_catalog_reset_config_unchanged(&paths, &config, &locked)?;
        (locked, bytes)
    };
    let mut receipt = if let Some((active_request, _, _)) = catalog_reset_fence_evidence(&paths)? {
        if let Some(receipt) =
            load_receipt(&paths)?.filter(|receipt| receipt.request_digest == active_request)
        {
            verify_receipt(&paths, &receipt, &bytes, &allow, whole)?;
            receipt
        } else {
            let receipt = prepared(&paths, &bytes, &allow, whole)?;
            begin_or_resume_catalog_reset_fence(
                &paths,
                &receipt.request_digest,
                &receipt.preflight_token,
                &receipt.preflight_token,
            )?;
            archive(&paths, receipt)?
        }
    } else {
        let receipt = prepared(&paths, &bytes, &allow, whole)?;
        if expected.is_some_and(|v| v != receipt.preflight_token) {
            return Err(refuse("reset authority changed after preflight"));
        }
        begin_or_resume_catalog_reset_fence(
            &paths,
            &receipt.request_digest,
            &receipt.preflight_token,
            &receipt.preflight_token,
        )?;
        archive(&paths, receipt)?
    };
    if expected.is_some_and(|v| v != receipt.preflight_token) {
        return Err(refuse("expected reset token differs from durable receipt"));
    }
    begin_or_resume_catalog_reset_fence(
        &paths,
        &receipt.request_digest,
        &receipt.preflight_token,
        &receipt.preflight_token,
    )?;
    hook(receipt.phase)?;
    if receipt.phase == Phase::Archived {
        CalibrationControlStore::open(&paths.calibration_dir)?
            .record_all_maps_evicted("catalog_reset")?;
        for directory in [&paths.audit_dir, &paths.journal_dir, &paths.tape_cache_dir] {
            if directory.exists() {
                fs::remove_dir_all(directory)
                    .map_err(|e| StateError::io_at("clear reset directory", directory, e))?;
            }
            crate::authority_manifest::create_durable_directories(directory)?;
            if let Some(parent) = directory.parent() {
                sync_directory(parent)?;
            }
        }
        remove_file_if_exists(&paths.sqlite_path)?;
        remove_sqlite_sidecars(&paths.sqlite_path)?;
        for name in ["authority-manifest.json", "authority-format"] {
            remove_file_if_exists(&paths.state_dir.join(name))?;
        }
        sync_directory(&paths.state_dir)?;
        if let Some(parent) = paths.sqlite_path.parent() {
            crate::authority_manifest::create_durable_directories(parent)?;
            sync_directory(parent)?;
        }
        receipt.phase = Phase::Cleared;
        publish_receipt(&paths, &receipt)?;
        hook(receipt.phase)?;
    }
    let manifest = AuthorityManifest::install_reset(&paths, &lease, receipt.new_library_uuid)?;
    if receipt.phase == Phase::Cleared {
        receipt.phase = Phase::Manifest;
        publish_receipt(&paths, &receipt)?;
        hook(receipt.phase)?;
    }
    boundary(&paths, lease.clone(), manifest.clone(), &receipt, &config)?;
    if receipt.phase == Phase::Manifest {
        receipt.phase = Phase::Boundary;
        publish_receipt(&paths, &receipt)?;
        hook(receipt.phase)?;
    }
    if matches!(receipt.phase, Phase::Boundary | Phase::Complete) {
        // The library source was verified above. SQL may have disappeared or
        // been corrupted after an interrupted reset; rebuild it from that source.
        remove_file_if_exists(&paths.sqlite_path)?;
        remove_sqlite_sidecars(&paths.sqlite_path)?;
        let runtime = LibraryRuntime::open_bound_manifest(&paths, true, lease.clone(), manifest)
            .map_err(|e| refuse(e.to_string()))?;
        // Current configuration is an explicit source, never restored SQL.
        install_configuration(&runtime, &config)?;
        drop(runtime);
        if receipt.phase == Phase::Boundary {
            hook(Phase::Boundary)?;
        }
        crate::authority_layout::install(&paths, true)?;
        sync_regular_file(&paths.sqlite_path)?;
        if let Some(parent) = paths.sqlite_path.parent() {
            sync_directory(parent)?;
        }
        receipt.phase = Phase::Complete;
        publish_receipt(&paths, &receipt)?;
        hook(receipt.phase)?;
    }
    clear_catalog_reset_fence(&paths)?;
    Ok(CatalogResetReport {
        preserved_tapes: Vec::new(),
    })
}
pub(super) fn configuration_record(
    config: &RemConfig,
) -> Result<lib::ConfigurationAccepted, StateError> {
    let definitions = lib::Definitions {
        pools: lib::Bounded(
            config
                .tape_pools
                .iter()
                .map(|p| lib::PoolDefinition {
                    id: p.id.as_str().into(),
                    display_name: p.display_name.as_deref().map(Into::into),
                    copy_class: p.copy_class.as_deref().map(Into::into),
                    content_class: p.content_class.as_deref().map(Into::into),
                    scheme: p.scheme.as_deref().map(Into::into),
                    selection_policy: match p.selection_policy {
                        crate::config::PoolSelectionPolicyName::CompleteOrFill => {
                            lib::SelectionPolicy::CompleteOrFill
                        }
                        crate::config::PoolSelectionPolicyName::FillOldest => {
                            lib::SelectionPolicy::FillOldest
                        }
                    },
                    watermark_low_bits: p.watermark_low.to_bits(),
                    watermark_high_bits: p.watermark_high.to_bits(),
                    capacity_cap_bytes: p.capacity_cap_bytes,
                    block_size_bytes: p.block_size_bytes,
                    min_object_size_bytes: p.min_object_size_bytes,
                })
                .collect(),
        ),
        rules: lib::Bounded(
            config
                .tape_pool_rules
                .iter()
                .map(|r| lib::PoolRule {
                    prefix: r.prefix.as_str().into(),
                    pool_id: r.pool_id.as_str().into(),
                })
                .collect(),
        ),
        cleaning_prefixes: lib::Bounded(
            config
                .cleaning
                .voltag_prefixes
                .iter()
                .map(|v| v.as_str().into())
                .collect(),
        ),
    };
    let config_digest =
        lib::codec::configuration_digest(&definitions).map_err(|e| refuse(e.to_string()))?;
    Ok(lib::ConfigurationAccepted {
        generation: 1,
        config_digest,
        definitions,
    })
}
fn install_configuration(runtime: &LibraryRuntime, config: &RemConfig) -> Result<(), StateError> {
    let value = configuration_record(config)?;
    if let Some(prior) = runtime.configuration() {
        if prior == value {
            return Ok(());
        }
        return Err(refuse("reset configuration differs from pinned source"));
    }
    runtime
        .append(|a, v| {
            a.configuration_accepted(value, lib::AuditActor::System, lib::SourceLayer::Layer4, v)
        })
        .map_err(|e| refuse(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, StatePaths, RemConfig) {
        let root = tempfile::tempdir().unwrap();
        let text = format!(
            r#"[daemon]
state_dir = "{0}"
default_idle_timeout_seconds = 1800
io_memory_ceiling = "64 MiB"
append_ring_bytes = "16 MiB"
[tape_io]
read_reservoir_bytes = "16 MiB"
[journal]
dir = "{0}/journals"
require_trusted_volume = false
[audit]
dir = "{0}/audit"
fsync = true
[index]
sqlite_path = "{0}/catalog.sqlite"
[cache]
tape_catalog_dir = "{0}/cache"
[[tape_pools]]
id = "pool"
block_size = 262144
"#,
            root.path().display()
        );
        let config_path = root.path().join("config.toml");
        fs::write(&config_path, &text).unwrap();
        let config = parse_config_toml(&text).unwrap();
        let paths = StatePaths::from_config(config_path, &config);
        (root, paths, config)
    }
    fn seed(paths: &StatePaths) -> [u8; 16] {
        seed_label(paths, Some("EXACT"))
    }
    fn seed_label(paths: &StatePaths, label: Option<&str>) -> [u8; 16] {
        let lease = Arc::new(StateLockGuard::acquire(&paths.state_dir).unwrap());
        let runtime = LibraryRuntime::open(paths, true, lease).unwrap();
        let uuid = runtime.manifest().library_uuid();
        runtime
            .append(|a, v| {
                a.reprovision(
                    lib::Reprovision {
                        pool_source: Some(lib::PoolSource::Operator(lib::Unit)),
                        operation: None,
                        replaced_bot: None,
                        tape: lib::TapeRef {
                            uuid: [0x71; 16],
                            incarnation: 1,
                        },
                        predecessor: None,
                        mode: lib::ProvisionMode::Initialize,
                        label: label.map(Into::into),
                        label_provenance: label.map(|_| lib::LabelProvenance::Operator),
                        parity_scheme: None,
                        medium: lib::MediumSpec {
                            family: lib::MediumFamily::Lto(crate::LtoGen::Lto9),
                            usable_logical_bytes: 18_000_000_000_000,
                            record_size: 262144,
                            rerecord: true,
                            lifecycle_ops: true,
                            source: lib::GenerationSource::Operator,
                        },
                        pool: Some("pool".into()),
                        endpoint: None,
                        adoption: None,
                    },
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer4,
                    v,
                )
            })
            .unwrap();
        uuid
    }
    #[test]
    fn reset_phase_cuts_reuse_archive_identity_and_refuse_startup() {
        for cut in [
            Phase::Archived,
            Phase::Cleared,
            Phase::Manifest,
            Phase::Boundary,
            Phase::Complete,
        ] {
            let (_root, paths, config) = fixture();
            let old = seed(&paths);
            let original_manifest =
                fs::read(paths.state_dir.join("authority-manifest.json")).unwrap();
            // Any-version SQL is opaque and replaceable, even corrupt bytes.
            fs::write(&paths.sqlite_path, b"opaque not-a-sqlite database").unwrap();
            let allow = vec!["EXACT".to_string()];
            let error = execute_with_hook(
                paths.clone(),
                config.clone(),
                &[],
                &allow,
                None,
                false,
                |phase| {
                    if phase == cut {
                        Err(refuse(format!("injected durable cut {phase:?}")))
                    } else {
                        Ok(())
                    }
                },
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("injected durable cut"),
                "{cut:?}: {error}"
            );
            let before = load_receipt(&paths).unwrap().unwrap();
            assert_ne!(before.new_library_uuid, old);
            assert_eq!(
                fs::read(before.archive.join("authority-manifest.json")).unwrap(),
                original_manifest
            );
            assert_eq!(
                fs::read(before.archive.join("catalog.sqlite")).unwrap(),
                b"opaque not-a-sqlite database"
            );
            assert!(StateHandle::open_with_config(paths.clone(), config.clone()).is_err());
            execute(
                paths.clone(),
                config,
                &[],
                &allow,
                Some(&before.preflight_token),
                false,
            )
            .unwrap();
            let after = load_receipt(&paths).unwrap().unwrap();
            assert_eq!(after.phase, Phase::Complete);
            assert_eq!(after.archive, before.archive);
            assert_eq!(after.archive_digest, before.archive_digest);
            assert_eq!(after.new_library_uuid, before.new_library_uuid);
            assert_eq!(
                fs::read_dir(paths.state_dir.join(RESET_ARCHIVE_DIR))
                    .unwrap()
                    .count(),
                1
            );
            let manifest = AuthorityManifest::preflight(&paths).unwrap().unwrap();
            assert_eq!(manifest.library_uuid(), before.new_library_uuid);
            let view =
                lib::native_view::NativeTapeView::probe(manifest.tape_root(), &paths.sqlite_path)
                    .unwrap();
            let replay =
                lib::store::replay(manifest.library_root(), manifest.library_uuid(), &view)
                    .unwrap();
            assert_eq!(replay.last_sequence, 2);
            assert!(replay.fold.tapes.is_empty());
            assert_eq!(replay.fold.configuration().unwrap().generation, 1);
        }
    }
    #[test]
    fn reset_exact_erase_scope_comes_from_typed_authority_not_sql() {
        let (_root, paths, config) = fixture();
        seed(&paths);
        fs::write(&paths.sqlite_path, b"SQL inventory must never be read").unwrap();
        let before = source_digest(&paths).unwrap();
        let error = execute(
            paths.clone(),
            config.clone(),
            &[],
            &["OTHER".into()],
            None,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("outside exact allow-erase"));
        assert_eq!(source_digest(&paths).unwrap(), before);
        assert!(!paths.state_dir.join(RESET_ARCHIVE_DIR).exists());
        execute(paths.clone(), config, &[], &["EXACT".into()], None, false).unwrap();
        assert!(crate::CatalogReader::open(&paths.sqlite_path)
            .unwrap()
            .list_tapes(None, crate::TapeKindFilter::All)
            .unwrap()
            .is_empty());
    }
    #[test]
    fn reset_retry_refuses_changed_surviving_authority() {
        let (_root, paths, config) = fixture();
        seed(&paths);
        let allow = vec!["EXACT".into()];
        execute_with_hook(
            paths.clone(),
            config.clone(),
            &[],
            &allow,
            None,
            false,
            |phase| {
                if phase == Phase::Archived {
                    Err(refuse("cut"))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        let foreign = paths.audit_dir.join("new-authority");
        fs::write(&foreign, b"not archived").unwrap();
        assert!(execute(paths.clone(), config, &[], &allow, None, false).is_err());
        assert_eq!(fs::read(&foreign).unwrap(), b"not archived");
        assert!(catalog_reset_fence_path(&paths).exists());
    }
    #[test]
    fn reset_refuses_sql_sidecar_symlink_before_lock_or_archive() {
        let (_root, paths, config) = fixture();
        let outside = paths.state_dir.join("outside");
        fs::write(&outside, b"preserve").unwrap();
        std::os::unix::fs::symlink(
            &outside,
            PathBuf::from(format!("{}-wal", paths.sqlite_path.display())),
        )
        .unwrap();
        assert!(execute(paths.clone(), config, &[], &[], None, true).is_err());
        assert!(!catalog_reset_fence_path(&paths).exists());
        assert!(!paths.state_dir.join(RESET_ARCHIVE_DIR).exists());
        assert_eq!(fs::read(outside).unwrap(), b"preserve");
    }
    #[test]
    fn reset_refuses_nested_calibration_symlink_before_mutation() {
        let (_root, paths, config) = fixture();
        let outside = paths.state_dir.join("outside-calibration");
        fs::write(&outside, b"preserved control authority").unwrap();
        fs::create_dir_all(&paths.calibration_dir).unwrap();
        std::os::unix::fs::symlink(&outside, paths.calibration_dir.join("control")).unwrap();
        assert!(execute(paths.clone(), config, &[], &[], None, true).is_err());
        assert!(!paths.state_dir.join("state.lock").exists());
        assert!(!catalog_reset_fence_path(&paths).exists());
        assert!(!paths.state_dir.join(RESET_ARCHIVE_DIR).exists());
        assert_eq!(fs::read(outside).unwrap(), b"preserved control authority");
    }
    #[test]
    fn reset_receipt_retry_survives_cut_before_rename() {
        let (_root, paths, config) = fixture();
        seed(&paths);
        execute_with_hook(
            paths.clone(),
            config,
            &[],
            &["EXACT".into()],
            None,
            false,
            |phase| {
                if phase == Phase::Archived {
                    Err(refuse("archive cut"))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        let old_bytes = fs::read(paths.state_dir.join(RECEIPT)).unwrap();
        let receipt = load_receipt(&paths).unwrap().unwrap();
        let mut abandoned = None;
        publish_receipt_before_rename(&paths, &receipt, |temporary| {
            temporary.keep = true;
            abandoned = Some(temporary.path.clone());
            Err(refuse("cut before receipt rename"))
        })
        .unwrap_err();
        let abandoned = abandoned.unwrap();
        assert!(abandoned.exists());
        assert_eq!(fs::read(paths.state_dir.join(RECEIPT)).unwrap(), old_bytes);
        publish_receipt(&paths, &receipt).unwrap();
        assert_eq!(fs::read(paths.state_dir.join(RECEIPT)).unwrap(), old_bytes);
        assert!(abandoned.exists(), "retry must use a fresh temporary name");
    }
    #[test]
    fn reset_preflight_refuses_previous_receipt_for_new_fence() {
        let (_root, paths, config) = fixture();
        seed(&paths);
        execute(paths.clone(), config, &[], &["EXACT".into()], None, false).unwrap();
        let previous = load_receipt(&paths).unwrap().unwrap();
        let next = prepared(&paths, &fs::read(&paths.config_path).unwrap(), &[], false).unwrap();
        assert_ne!(previous.request_digest, next.request_digest);
        begin_or_resume_catalog_reset_fence(
            &paths,
            &next.request_digest,
            &next.preflight_token,
            &next.preflight_token,
        )
        .unwrap();
        assert!(preflight(&paths.config_path, &[], &[])
            .unwrap_err()
            .to_string()
            .contains("no durable archive receipt"));
        assert_eq!(
            load_receipt(&paths).unwrap().unwrap().request_digest,
            previous.request_digest
        );
    }
    #[test]
    fn reset_scoped_refuses_orphan_native_but_whole_archives_it() {
        use crate::authority::TapeAuthorityWriter;
        for corrupt in [false, true] {
            let (_root, paths, config) = fixture();
            let lease = Arc::new(StateLockGuard::acquire(&paths.state_dir).unwrap());
            let runtime = LibraryRuntime::open(&paths, true, lease).unwrap();
            let id = [0xa7; 16];
            let native =
                crate::TapeAuthority::open_fixture(paths.journal_dir.join("tapes"), id).unwrap();
            let mut owner = native.acquire_exclusive().unwrap();
            owner
                .publish_report(&(&crate::checkpoint::tests::record(id)).into())
                .unwrap();
            drop(owner);
            drop(native);
            drop(runtime);
            if corrupt {
                fs::write(
                    paths
                        .journal_dir
                        .join("tapes")
                        .join(format!("{}.remtape", uuid::Uuid::from_bytes(id).simple())),
                    b"corrupt native header",
                )
                .unwrap();
            }
            let before = source_digest(&paths).unwrap();
            let error = execute(paths.clone(), config.clone(), &[], &[], None, false).unwrap_err();
            assert!(error.to_string().contains("Reprovision"), "{error}");
            assert_eq!(source_digest(&paths).unwrap(), before);
            assert!(!catalog_reset_fence_path(&paths).exists());
            execute(paths.clone(), config, &[], &[], None, true).unwrap();
            let receipt = load_receipt(&paths).unwrap().unwrap();
            assert!(receipt
                .archive
                .join("journals/tapes")
                .join(format!("{}.remtape", uuid::Uuid::from_bytes(id).simple()))
                .is_file());
        }
    }

    #[test]
    fn reset_whole_state_accepts_live_unlabelled_but_scoped_refuses() {
        let (_root, paths, config) = fixture();
        seed_label(&paths, None);
        let before = test_authority_bytes(&paths);
        assert!(
            execute(paths.clone(), config.clone(), &[], &[], None, false)
                .unwrap_err()
                .to_string()
                .contains("unlabelled live")
        );
        assert_eq!(test_authority_bytes(&paths), before);
        execute(paths.clone(), config, &[], &[], None, true).unwrap();
        let receipt = load_receipt(&paths).unwrap().unwrap();
        assert_eq!(receipt.tapes.len(), 1);
        assert_eq!(receipt.tapes[0].voltag, None);
        assert_eq!(receipt.phase, Phase::Complete);
    }
    #[test]
    fn reset_whole_state_accepts_cleaning_but_scoped_omission_refuses() {
        let (_root, paths, config) = fixture();
        let lease = Arc::new(StateLockGuard::acquire(&paths.state_dir).unwrap());
        let runtime = LibraryRuntime::open(&paths, true, lease).unwrap();
        let barcode: lib::LabelText = "CLEAN001".into();
        runtime
            .append(|a, v| {
                a.cleaning_recorded(
                    lib::CleaningRecorded {
                        run_id: "registration".into(),
                        previous: None,
                        drive_uuid: None,
                        cartridge: Some(lib::cleaning_cartridge_ref(&barcode)),
                        action: lib::CleaningAction::Register(lib::Register {
                            barcode,
                            home_slot: 0x400,
                        }),
                    },
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer4,
                    v,
                )
            })
            .unwrap();
        drop(runtime);
        let before = test_authority_bytes(&paths);
        assert!(
            execute(paths.clone(), config.clone(), &[], &[], None, false)
                .unwrap_err()
                .to_string()
                .contains("cleaning cartridge")
        );
        assert_eq!(test_authority_bytes(&paths), before);
        execute(paths.clone(), config, &[], &[], None, true).unwrap();
        assert_eq!(
            load_receipt(&paths).unwrap().unwrap().tapes[0].kind,
            "cleaning"
        );
    }
    #[test]
    fn reset_preserves_retired_unlabelled_selector_exemption() {
        struct EmptyNative;
        impl lib::authority::FinalizationView for EmptyNative {
            fn committed_instance_count(&self, _: lib::TapeRef) -> u64 {
                0
            }
            fn finalization_blocks_change(&self, _: lib::TapeRef) -> bool {
                false
            }
        }
        let (_root, paths, config) = fixture();
        seed(&paths);
        let lease = Arc::new(StateLockGuard::acquire(&paths.state_dir).unwrap());
        let runtime = LibraryRuntime::open(&paths, true, lease).unwrap();
        runtime
            .append(|a, v| {
                a.retire(
                    lib::Retire {
                        tape: lib::TapeRef {
                            uuid: [0x71; 16],
                            incarnation: 1,
                        },
                        reason: lib::RetireReason::OperatorRequested,
                        observed_tape_sequence: None,
                        assignment_generation: 1,
                        detail: None,
                    },
                    1,
                    &EmptyNative,
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer4,
                    v,
                )
            })
            .unwrap();
        drop(runtime);
        let tapes = source(&paths, &[], false).unwrap().0;
        assert_eq!(tapes.len(), 1);
        assert_eq!(tapes[0].state, "retired");
        assert_eq!(tapes[0].voltag, None);
        execute(paths, config, &[], &[], None, false).unwrap();
    }
    #[test]
    fn reset_whole_state_archives_legacy_but_scoped_legacy_refuses() {
        let (_root, paths, config) = fixture();
        fs::create_dir_all(&paths.audit_dir).unwrap();
        let mut audit = FileAuditLog::open(&paths.audit_dir, true).unwrap();
        audit
            .append(AuditEventRecord {
                actor: AuditActor::System,
                source_layer: SourceLayer::Layer4,
                operation_id: None,
                session_id: None,
                idempotency_key: None,
                event: AuditEvent::RecoveryEvent,
                subject: AuditSubject {
                    kind: "legacy".into(),
                    id: None,
                },
                detail: BTreeMap::new(),
            })
            .unwrap();
        drop(audit);
        let old_hash = FileAuditLog::archive_terminal_hash(&paths.audit_dir).unwrap();
        let before = source_digest(&paths).unwrap();
        assert!(execute(
            paths.clone(),
            config.clone(),
            &[],
            &["EXACT".into()],
            None,
            false
        )
        .is_err());
        assert_eq!(source_digest(&paths).unwrap(), before);
        execute(paths.clone(), config, &[], &[], None, true).unwrap();
        let receipt = load_receipt(&paths).unwrap().unwrap();
        assert_eq!(receipt.old_terminal_hash, old_hash);
        assert!(receipt.old_library_uuid.is_none());
        assert!(receipt.archive.join("audit").is_dir());
        assert!(AuthorityManifest::preflight(&paths).unwrap().is_some());
    }
    #[test]
    fn reset_complete_retry_refuses_missing_boundary_and_keeps_fence() {
        let (_root, paths, config) = fixture();
        seed(&paths);
        let allow = vec!["EXACT".into()];
        execute_with_hook(
            paths.clone(),
            config.clone(),
            &[],
            &allow,
            None,
            false,
            |phase| {
                if phase == Phase::Complete {
                    Err(refuse("cut after complete receipt"))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        for entry in fs::read_dir(&paths.audit_dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|v| v == "remlib") {
                fs::remove_file(path).unwrap();
            }
        }
        assert!(execute(paths.clone(), config, &[], &allow, None, false).is_err());
        assert!(catalog_reset_fence_path(&paths).exists());
    }
    #[test]
    fn reset_config_publication_retry_does_not_duplicate_source() {
        let (_root, paths, config) = fixture();
        seed(&paths);
        let allow = vec!["EXACT".into()];
        let mut cuts = 0;
        execute_with_hook(
            paths.clone(),
            config.clone(),
            &[],
            &allow,
            None,
            false,
            |phase| {
                if phase == Phase::Boundary {
                    cuts += 1;
                    if cuts == 2 {
                        return Err(refuse("cut after config append before receipt"));
                    }
                }
                Ok(())
            },
        )
        .unwrap_err();
        let receipt = load_receipt(&paths).unwrap().unwrap();
        assert_eq!(receipt.phase, Phase::Boundary);
        execute(paths.clone(), config, &[], &allow, None, false).unwrap();
        let manifest = AuthorityManifest::preflight(&paths).unwrap().unwrap();
        let view =
            lib::native_view::NativeTapeView::probe(manifest.tape_root(), &paths.sqlite_path)
                .unwrap();
        let replay =
            lib::store::replay(manifest.library_root(), manifest.library_uuid(), &view).unwrap();
        assert_eq!(replay.last_sequence, 2);
    }
    #[test]
    fn reset_sidecars_cannot_alias_preserved_paths_or_authority() {
        for collision in ["config", "calibration", "audit"] {
            let (_root, mut paths, config) = fixture();
            let alias = PathBuf::from(format!("{}-wal", paths.sqlite_path.display()));
            match collision {
                "config" => {
                    fs::copy(&paths.config_path, &alias).unwrap();
                    paths.config_path = alias.clone();
                }
                "calibration" => paths.calibration_dir = alias.clone(),
                _ => paths.audit_dir = alias.clone(),
            }
            let original = fs::read(&paths.config_path).unwrap();
            assert!(execute(paths.clone(), config, &[], &[], None, true).is_err());
            assert_eq!(fs::read(&paths.config_path).unwrap(), original);
            assert!(!paths.state_dir.join("state.lock").exists());
            assert!(!catalog_reset_fence_path(&paths).exists());
        }
    }
    #[test]
    fn reset_preflight_and_resume_bind_config_bytes() {
        let (_root, paths, config) = fixture();
        seed(&paths);
        let allow = vec!["EXACT".into()];
        let admitted = preflight(&paths.config_path, &[], &allow).unwrap();
        let original = fs::read(&paths.config_path).unwrap();
        let mut edited = original.clone();
        edited.extend_from_slice(b"\n# changed after preflight\n");
        fs::write(&paths.config_path, &edited).unwrap();
        assert!(execute(
            paths.clone(),
            config.clone(),
            &[],
            &allow,
            Some(&admitted.preflight_token),
            false
        )
        .is_err());
        assert!(!catalog_reset_fence_path(&paths).exists());
        assert!(!paths.state_dir.join(RESET_ARCHIVE_DIR).exists());
        fs::write(&paths.config_path, &original).unwrap();
        execute_with_hook(
            paths.clone(),
            config.clone(),
            &[],
            &allow,
            None,
            false,
            |phase| {
                if phase == Phase::Archived {
                    Err(refuse("cut after archive"))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        fs::write(&paths.config_path, &edited).unwrap();
        assert!(execute(paths.clone(), config, &[], &allow, None, false).is_err());
        assert!(catalog_reset_fence_path(&paths).exists());
        assert_eq!(fs::read(&paths.config_path).unwrap(), edited);
    }
    /// Explicit tempdir measurement of current source replay and archive work.
    /// RSS is process high-water (including setup), not attributed only to reset.
    #[test]
    #[ignore = "explicit rebuild/reset measurement"]
    fn rebuild_reset_cutover_measurement() {
        let (_root, paths, config) = fixture();
        let mut state = StateHandle::open_with_config(paths.clone(), config.clone()).unwrap();
        let runtime = state.library_runtime();
        for generation in 1..=500 {
            for drive in [1u8, 2] {
                runtime
                    .append(|a, v| {
                        a.drive_observed(
                            lib::DriveObserved {
                                drive_uuid: [drive; 16],
                                serial: Some(format!("MEASURE-{drive}").as_str().into()),
                                identity_source: lib::IdentitySource::DvcidInline,
                                endpoint: Some(lib::DriveKey::Bay(lib::Bay {
                                    library_serial: "MEASURE".into(),
                                    bay: 0x100 + u16::from(drive),
                                })),
                                managed: lib::Managed::Rem,
                                inventory_generation: generation,
                                vendor: Some("MODEL".into()),
                                product: Some("MEASURE".into()),
                                firmware_rev: None,
                            },
                            lib::AuditActor::System,
                            lib::SourceLayer::Layer4,
                            v,
                        )
                    })
                    .unwrap();
            }
        }
        let begin = std::time::Instant::now();
        let report = state.rebuild_index_from_journals().unwrap();
        let rebuild_seconds = begin.elapsed().as_secs_f64();
        assert_eq!(state.catalog().list_drives(true, true).unwrap().len(), 2);
        drop(runtime);
        drop(state);
        let begin = std::time::Instant::now();
        execute(paths.clone(), config, &[], &[], None, true).unwrap();
        let reset_seconds = begin.elapsed().as_secs_f64();
        let receipt = load_receipt(&paths).unwrap().unwrap();
        fn bytes(path: &Path) -> u64 {
            fs::read_dir(path)
                .unwrap()
                .map(|e| {
                    let e = e.unwrap();
                    let m = e.metadata().unwrap();
                    if m.is_dir() {
                        bytes(&e.path())
                    } else {
                        m.len()
                    }
                })
                .sum()
        }
        let archived_bytes = bytes(&receipt.archive);
        let rss = fs::read_to_string("/proc/self/status")
            .unwrap()
            .lines()
            .find(|line| line.starts_with("VmHWM:"))
            .unwrap()
            .to_owned();
        println!("CUTOVER_REBUILD_RESET source_observations=1000 rebuild_seconds={rebuild_seconds:.6} reset_seconds={reset_seconds:.6} archived_bytes={archived_bytes} process_{rss} report={report:?}");
    }
}
