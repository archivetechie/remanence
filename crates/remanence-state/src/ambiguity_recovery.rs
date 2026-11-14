//! Offline inspection and explicit recovery of host-log ambiguity. Recovery owns one state
//! lease, checks space and library damage before library writes, clears native markers first,
//! then opens the runtime once to reconcile owed fences and record the retained authority.
use crate::{
    authority_manifest::AuthorityManifest,
    library_log::{
        self as lib,
        native_view::NativeTapeView,
        store::{self, Clock, Origin, Replay, UuidSource},
    },
    library_runtime::LibraryRuntime,
    log_frame::publication,
    tape_log::store::Store,
    CatalogReader, StateError, StateLockGuard, StatePaths,
};
use nix::fcntl::{Flock, FlockArg};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Damage needs the later repair protocol; clearing ambiguity cannot repair a refused replay.
pub const LIBRARY_LOG_REPAIR_DEFERRED: &str = "LibraryLogRepairDeferred";

fn failure(context: &str, error: impl std::fmt::Display) -> StateError {
    StateError::JournalReplayFailed(format!("{context}: {error}"))
}
fn io_failure(error: io::Error) -> StateError {
    failure("ambiguity recovery", error)
}
fn canonical_existing(path: &Path) -> io::Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(error) => Err(error),
    }
}
fn entries(path: &Path) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut paths = entries
        .map(|entry| entry.map(|e| e.path()))
        .collect::<io::Result<Vec<_>>>()?;
    paths.sort();
    Ok(paths)
}
struct TapeMarker {
    id: lib::Id,
    path: PathBuf,
    backed: bool,
}
struct Inventory {
    library_root: PathBuf,
    tape_root: PathBuf,
    library_marked: bool,
    tapes: Vec<TapeMarker>,
    segments: Vec<(PathBuf, u64)>,
    trigger: bool,
    owed: Vec<lib::Id>,
}
impl Inventory {
    fn markers(&self) -> Vec<String> {
        let mut markers: Vec<_> = self
            .tapes
            .iter()
            .map(|t| t.path.with_extension("ambiguous").display().to_string())
            .collect();
        if self.library_marked {
            markers.push(
                self.library_root
                    .join("library-log.ambiguous")
                    .display()
                    .to_string(),
            );
        }
        markers
    }
    fn blocked_by_tape(&self) -> bool {
        self.tapes.iter().any(|t| t.backed)
    }
}
/// Use the same canonical identities as runtime publication, including process-only fences.
fn marker_inventory(
    paths: &StatePaths,
    manifest: Option<&AuthorityManifest>,
) -> Result<Inventory, StateError> {
    let (library_root, tape_root) = match manifest {
        Some(m) => (m.library_root().to_path_buf(), m.tape_root().to_path_buf()),
        None => (
            canonical_existing(&paths.audit_dir).map_err(io_failure)?,
            canonical_existing(&paths.journal_dir.join("tapes")).map_err(io_failure)?,
        ),
    };
    let library_marked =
        publication::has_ambiguity(&library_root.join("library-log")).map_err(io_failure)?;
    let mut ids = BTreeSet::new();
    for path in entries(&tape_root).map_err(io_failure)? {
        if path
            .extension()
            .is_some_and(|e| e == "remtape" || e == "ambiguous")
        {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| failure("native marker", "invalid UUID filename"))?;
            let id = uuid::Uuid::parse_str(stem).map_err(|e| failure("native marker", e))?;
            if stem != id.simple().to_string() {
                return Err(failure("native marker", "noncanonical UUID filename"));
            }
            ids.insert(*id.as_bytes());
        }
    }
    let mut tapes = Vec::new();
    for id in ids {
        let path = tape_root.join(format!("{}.remtape", uuid::Uuid::from_bytes(id).simple()));
        if publication::has_ambiguity(&path).map_err(io_failure)? {
            let backed = path.try_exists().map_err(io_failure)?;
            tapes.push(TapeMarker { id, path, backed });
        }
    }
    Ok(Inventory {
        library_root,
        tape_root,
        library_marked,
        tapes,
        segments: Vec::new(),
        trigger: false,
        owed: Vec::new(),
    })
}
fn inventory(
    paths: &StatePaths,
    manifest: Option<&AuthorityManifest>,
) -> Result<Inventory, StateError> {
    let mut inventory = marker_inventory(paths, manifest)?;
    let library_root = &inventory.library_root;
    let segments = entries(library_root)
        .map_err(io_failure)?
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "remlib"))
        .map(|path| fs::metadata(&path).map(|m| (path, m.len())))
        .collect::<io::Result<Vec<_>>>()
        .map_err(io_failure)?;
    let trigger_path = store::fence_reconcile_path(library_root);
    let (trigger, mut owed) = match fs::read(&trigger_path) {
        Ok(bytes) => {
            if bytes.len() % 16 != 0 {
                return Err(failure("fence-reconcile", "malformed entries"));
            }
            (
                true,
                bytes
                    .chunks_exact(16)
                    .map(|v| v.try_into().unwrap())
                    .collect::<Vec<lib::Id>>(),
            )
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => (false, Vec::new()),
        Err(error) => return Err(io_failure(error)),
    };
    owed.sort_unstable();
    owed.dedup();
    inventory.segments = segments;
    inventory.trigger = trigger;
    inventory.owed = owed;
    Ok(inventory)
}

/// Probe only the existing lock inode, with no create, write, or diagnostics rewrite.
fn daemon_running(paths: &StatePaths) -> Result<bool, StateError> {
    let path = paths.state_dir.join("state.lock");
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(StateError::io_at("inspect state lock", path, error)),
    };
    match Flock::lock(file, FlockArg::LockSharedNonblock) {
        Ok(_lease) => Ok(false),
        Err((_, nix::errno::Errno::EWOULDBLOCK)) => Ok(true),
        Err((_, error)) => Err(StateError::io_at(
            "probe state lock",
            path,
            io::Error::from(error),
        )),
    }
}
fn free_bytes(path: &Path) -> io::Result<u64> {
    let stat = nix::sys::statvfs::statvfs(path).map_err(io::Error::from)?;
    stat.blocks_available()
        .checked_mul(stat.fragment_size())
        .ok_or_else(|| io::Error::other("free space overflow"))
}
#[derive(serde::Serialize)]
struct SpaceNeed {
    device: u64,
    roots: Vec<String>,
    needed_bytes: u64,
    available_bytes: u64,
}
/// Sum all retained copies per filesystem so roots on one device cannot double-count free bytes.
fn space_plan(
    paths: &StatePaths,
    inventory: &Inventory,
    probe: &impl Fn(&Path) -> io::Result<u64>,
) -> Result<Vec<SpaceNeed>, StateError> {
    let mut amounts = vec![
        (
            &inventory.library_root,
            if inventory.library_marked || !inventory.tapes.is_empty() || inventory.trigger {
                inventory.segments.last().map_or(0, |(_, size)| *size)
            } else {
                0
            },
        ),
        (&inventory.tape_root, 0),
    ];
    for tape in &inventory.tapes {
        let need = if tape.backed {
            Store::open(&inventory.tape_root, tape.id, &paths.sqlite_path)
                .and_then(|s| s.inspect_tail())
                .map_err(|e| failure(&tape.path.display().to_string(), e))?
                .valid_bytes
        } else {
            0
        };
        amounts.push((&inventory.tape_root, need));
    }
    let mut filesystems: BTreeMap<u64, SpaceNeed> = BTreeMap::new();
    for (root, need) in amounts {
        let metadata = match fs::metadata(root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound && need == 0 => continue,
            Err(error) => return Err(io_failure(error)),
        };
        let device = metadata.dev();
        let entry = match filesystems.entry(device) {
            std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::btree_map::Entry::Vacant(e) => e.insert(SpaceNeed {
                device,
                roots: Vec::new(),
                needed_bytes: 0,
                available_bytes: probe(root).map_err(io_failure)?,
            }),
        };
        let name = root.display().to_string();
        if !entry.roots.contains(&name) {
            entry.roots.push(name);
        }
        entry.needed_bytes = entry
            .needed_bytes
            .checked_add(need)
            .ok_or_else(|| failure("free space", "required bytes overflow"))?;
    }
    Ok(filesystems.into_values().collect())
}
fn replay(paths: &StatePaths, manifest: &AuthorityManifest) -> Result<Replay, StateError> {
    let view = NativeTapeView::probe(manifest.tape_root(), &paths.sqlite_path)
        .map_err(|e| failure("native probe", e))?;
    store::replay(manifest.library_root(), manifest.library_uuid(), &view)
        .map_err(|e| failure("library replay", e))
}
fn refusal_text(refusal: &store::Refusal) -> String {
    let sequence = refusal
        .sequence
        .map_or_else(|| "unknown".to_owned(), |sequence| sequence.to_string());
    format!("{LIBRARY_LOG_REPAIR_DEFERRED}: segment {} offset {} sequence {sequence}: {}; no repair in this release; reset-catalog refuses too; keep the state directory",refusal.segment,refusal.offset,refusal.fault)
}
fn checked_replay(paths: &StatePaths, manifest: &AuthorityManifest) -> Result<Replay, StateError> {
    let replay = replay(paths, manifest)?;
    if let Some(refusal) = &replay.refusal {
        return Err(StateError::JournalReplayFailed(refusal_text(refusal)));
    }
    Ok(replay)
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn library_end(replay: &Replay) -> Value {
    let last = replay.segments.last();
    json!({"last_sequence":replay.last_sequence,"terminal_hash":hex(&replay.terminal_hash),"segment":last.map(|s|&s.name),"through":last.map_or(0,|s|s.through),"tail":last.map(|s|format!("{:?}",s.tail))})
}
fn replay_report(replay: &Replay) -> Value {
    let mut report = library_end(replay);
    report["refusal"]=replay.refusal.as_ref().map_or(Value::Null,|r|json!({"segment":r.segment,"offset":r.offset,"sequence":r.sequence,"fault":r.fault.to_string(),"message":refusal_text(r)}));
    report
}

/// Inspect the configured authority without taking state ownership or opening a library writer.
pub fn inspect(config_path: impl AsRef<Path>) -> Result<Value, StateError> {
    let config = crate::load_config(&config_path)?;
    let paths = StatePaths::from_config(config_path, &config);
    let running = daemon_running(&paths)?;
    let preflight = AuthorityManifest::preflight(&paths);
    let manifest = preflight.as_ref().ok().and_then(|m| m.as_ref());
    let inventory = inventory(&paths, manifest)?;
    let spaces = space_plan(&paths, &inventory, &free_bytes);
    let skip = if running {
        Some("daemon is running".to_string())
    } else if inventory.blocked_by_tape() {
        Some("a marked .remtape blocks the native probe".to_string())
    } else if manifest.is_none() {
        Some("no admitted authority manifest".to_string())
    } else {
        None
    };
    let result = if skip.is_none() {
        Some(replay(&paths, manifest.unwrap()))
    } else {
        None
    };
    Ok(json!({
        "daemon_running":running,
        "markers":inventory.markers(),
        "fence_reconcile_exists":inventory.trigger,
        "owed_tape_uuids":inventory.owed.iter().map(|id|uuid::Uuid::from_bytes(*id).to_string()).collect::<Vec<_>>(),
        "segments":inventory.segments.iter().map(|(p,size)|json!({"path":p.display().to_string(),"size_bytes":size})).collect::<Vec<_>>(),
        "filesystems":spaces.as_ref().ok(),
        "space_error":spaces.as_ref().err().map(ToString::to_string),
        "preflight_error":preflight.as_ref().err().map(ToString::to_string),
        "library_replay":result.as_ref().and_then(|r|r.as_ref().ok()).map(replay_report),
        "library_replay_skipped":skip,
        "library_replay_error":result.as_ref().and_then(|r|r.as_ref().err()).map(ToString::to_string)
    }))
}

struct RecoveryClock;
impl Clock for RecoveryClock {
    fn now(&self) -> u64 {
        u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos()).unwrap_or(0)
    }
}
struct RecoveryUuids;
impl UuidSource for RecoveryUuids {
    fn next(&self, _: u64) -> lib::Id {
        *uuid::Uuid::new_v4().as_bytes()
    }
}

/// Recover the configured logs after the caller has confirmed this offline operation.
pub fn recover(config_path: impl AsRef<Path>) -> Result<Value, StateError> {
    let config = crate::load_config(&config_path)?;
    recover_with_space_probe(
        &StatePaths::from_config(config_path, &config),
        config.audit.fsync,
        free_bytes,
    )
}

/// Production and tests share the ordering and locking; only the free-byte observation varies.
pub(crate) fn recover_with_space_probe(
    paths: &StatePaths,
    fsync_diagnostics: bool,
    probe: impl Fn(&Path) -> io::Result<u64>,
) -> Result<Value, StateError> {
    let lease = Arc::new(StateLockGuard::acquire(&paths.state_dir)?);
    crate::state::ensure_no_catalog_reset_fence(paths)?;
    let Some(manifest) = AuthorityManifest::preflight(paths)? else {
        return Ok(
            json!({"cleared_markers":[],"retained_library_end":null,"reconciliation":[],"nothing_to_recover":true}),
        );
    };
    let inventory = inventory(paths, Some(&manifest))?;
    // No native snapshot probe (which may create lock files), cache open, or writer precedes space admission.
    for space in space_plan(paths, &inventory, &probe)? {
        if space.available_bytes < space.needed_bytes {
            return Err(failure(
                "free space",
                format!(
                    "filesystem {} needs {} bytes; {} available",
                    space.device, space.needed_bytes, space.available_bytes
                ),
            ));
        }
    }
    let before = if !inventory.blocked_by_tape() {
        Some(checked_replay(paths, &manifest)?)
    } else {
        None
    };
    let marks =
        if !inventory.tapes.is_empty() && paths.sqlite_path.try_exists().map_err(io_failure)? {
            CatalogReader::open(&paths.sqlite_path)?.tape_projection_marks()?
        } else {
            BTreeMap::new()
        };
    let mut cleared = Vec::new();
    for tape in &inventory.tapes {
        Store::open(manifest.tape_root(), tape.id, &paths.sqlite_path)
            .and_then(|s| s.clear_ambiguity_above(marks.get(&tape.id).map_or(0, |m| m.sequence)))
            .map_err(|e| failure(&tape.path.display().to_string(), e))?;
        cleared.push(tape.path.with_extension("ambiguous").display().to_string());
    }
    let retained = match before {
        Some(replay) => replay,
        None => checked_replay(paths, &manifest)?,
    };
    if publication::has_ambiguity(&manifest.library_root().join("library-log"))
        .map_err(io_failure)?
    {
        let view = NativeTapeView::probe(manifest.tape_root(), &paths.sqlite_path)
            .map_err(|e| failure("native probe", e))?;
        let mut log = lib::LibraryLog::open(
            manifest.library_root(),
            manifest.library_uuid(),
            Origin {
                host: "offline-ambiguity-recovery".into(),
                pid: std::process::id(),
                build: env!("CARGO_PKG_VERSION").into(),
            },
            Box::new(RecoveryClock),
            Box::new(RecoveryUuids),
            fsync_diagnostics,
            &view,
        )
        .map_err(|e| failure("open library recovery", e))?;
        log.clear_ambiguity(&view)
            .map_err(|e| failure("clear library ambiguity", e))?;
        drop(log);
        cleared.push(
            manifest
                .library_root()
                .join("library-log.ambiguous")
                .display()
                .to_string(),
        );
    }
    let mut reconciliation = Vec::new();
    if !cleared.is_empty() || inventory.trigger {
        let (runtime, summary) = LibraryRuntime::open_reporting(paths, true, lease.clone())
            .map_err(|e| failure("reopen after ambiguity recovery", e))?;
        reconciliation=summary.iter().map(|(id,outcome)|json!({"tape_uuid":uuid::Uuid::from_bytes(*id).to_string(),"outcome":outcome})).collect();
        let diagnostic = lib::Diagnostic {
            event: lib::DiagnosticEvent::RecoveryEvent,
            subject_kind: "ambiguity-recovery".into(),
            subject_id: None,
            detail: lib::Bounded(vec![
                lib::DiagnosticEntry(
                    "cleared_markers".into(),
                    lib::DiagnosticValue::Bytes(lib::Blob(
                        serde_json::to_vec(&cleared).map_err(|e| failure("recovery record", e))?,
                    )),
                ),
                lib::DiagnosticEntry(
                    "reconciliation".into(),
                    lib::DiagnosticValue::Bytes(lib::Blob(
                        serde_json::to_vec(&reconciliation)
                            .map_err(|e| failure("recovery record", e))?,
                    )),
                ),
                lib::DiagnosticEntry(
                    "retained_hash".into(),
                    lib::DiagnosticValue::Bytes(lib::Blob(retained.terminal_hash.to_vec())),
                ),
                lib::DiagnosticEntry(
                    "retained_segment".into(),
                    retained
                        .segments
                        .last()
                        .map_or(lib::DiagnosticValue::Null(lib::Unit), |s| {
                            lib::DiagnosticValue::Text(s.name.as_str().into())
                        }),
                ),
                lib::DiagnosticEntry(
                    "retained_sequence".into(),
                    lib::DiagnosticValue::Unsigned(retained.last_sequence),
                ),
                lib::DiagnosticEntry(
                    "retained_through".into(),
                    lib::DiagnosticValue::Unsigned(
                        retained.segments.last().map_or(0, |s| s.through),
                    ),
                ),
            ]),
        };
        runtime
            .append(|authority, view| {
                authority.diagnostic(
                    diagnostic,
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer4,
                    view,
                )
            })
            .map_err(|e| failure("append recovery record", e))?;
        drop(runtime);
    }
    Ok(
        json!({"cleared_markers":cleared,"retained_library_end":library_end(&retained),"reconciliation":reconciliation}),
    )
}

/// Explain ambiguity or damage while the caller already owns the state lease; never probe that lock.
pub fn startup_hint(paths: &StatePaths) -> Option<String> {
    let preflight = AuthorityManifest::preflight(paths);
    let manifest = preflight.as_ref().ok().and_then(|m| m.as_ref());
    let inventory = marker_inventory(paths, manifest).ok()?;
    let markers = inventory.markers();
    let mut lines = Vec::new();
    if !markers.is_empty() {
        lines.push(format!("Ambiguity markers: {}. A restart does not clear them. Stop the daemon, ensure free space, then run rem-debug library-log recover --config {} --confirm-clear-ambiguity",markers.join(", "),shell_path(&paths.config_path)));
    }
    if !inventory.blocked_by_tape() {
        if let Some(manifest) = manifest {
            if let Ok(replay) = replay(paths, manifest) {
                if let Some(refusal) = replay.refusal {
                    lines.push(refusal_text(&refusal));
                }
            }
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}
fn shell_path(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}
