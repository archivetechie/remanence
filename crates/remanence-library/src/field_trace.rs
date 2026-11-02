//! Field-kit-only command evidence. No payload bytes are retained. This records
//! transport completions (including raw CHECK CONDITION sense) before decoding.
use crate::transport::TransferOutcome;
use remanence_scsi::ScsiError;
use std::{
    fs::OpenOptions,
    io::Write,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(crate) fn record(
    drive: &serde_json::Value,
    cdb: &[u8],
    requested: usize,
    result: &Result<TransferOutcome, &ScsiError>,
    elapsed: Duration,
) {
    let Some(path) = std::env::var_os("REM_FIELD_TRACE") else {
        return;
    };
    if std::path::PathBuf::from(&path)
        .with_extension("jsonl.stop")
        .exists()
    {
        return;
    }
    static LOCK: Mutex<()> = Mutex::new(());
    static START: OnceLock<Instant> = OnceLock::new();
    let _guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (bytes, sense) = match result {
        Ok(o) => (o.bytes_transferred, None),
        #[cfg(target_os = "linux")]
        Err(ScsiError::CheckCondition {
            bytes_transferred,
            sense,
        }) => (*bytes_transferred, Some(sense)),
        _ => (0, None),
    };
    let value = serde_json::json!({"pid":std::process::id(),"drive":drive,"cdb":cdb,"requested_bytes":requested,
        "bytes_transferred":bytes,"sense_raw":sense,"result":format!("{result:?}"),
        "wall_ns":SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos().to_string(),
        "monotonic_ns":START.get_or_init(Instant::now).elapsed().as_nanos().to_string(),
        "duration_ns":elapsed.as_nanos().to_string()});
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        // A missing/truncated trace fails evidence validation in the kit.
        let _ = writeln!(f, "{value}");
        let _ = f.sync_data();
    }
}

/// Record a host stage separately from transport time, only in the field build.
pub fn stage(name: &str, tape: [u8; 16], epoch: u64, elapsed: Duration) {
    let Some(path) = std::env::var_os("REM_FIELD_TRACE") else {
        return;
    };
    let path = std::path::PathBuf::from(path);
    if path.with_extension("jsonl.stop").exists() {
        return;
    }
    let Some(parent) = path.parent() else {
        return;
    };
    let value = serde_json::json!({"stage": name, "tape_uuid_bytes": tape, "epoch_id": epoch,
        "pid": std::process::id(), "duration_ns": elapsed.as_nanos().to_string(),
        "wall_ns": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos().to_string()});
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(parent.join("host-stages.jsonl"))
    {
        let _ = writeln!(f, "{value}");
        let _ = f.sync_data();
    }
}
