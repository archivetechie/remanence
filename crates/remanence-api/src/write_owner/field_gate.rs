//! Owner-armed field-kit dispatch hold. Compiled only into field-kit binaries.
//! The one-shot arm binds the tape UUID and catalog barcode at object begin.
//! It is consumed then; the helper nonce correlates evidence, not daemon authority.
//! The hold begins after a real WRITE completes;
//! release never invents media completion and timeout refuses subsequent writes.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(Debug)]
pub(super) struct Gate {
    root: PathBuf,
    tape: [u8; 16],
    object: String,
    armed: Value,
    command_hash: Option<String>,
}
impl Gate {
    pub(super) fn load(
        tape: [u8; 16],
        barcode: Option<&str>,
        object: &str,
    ) -> Result<Option<Self>, String> {
        let Some(root) = std::env::var_os("REM_FIELD_GATE_DIR") else {
            return Ok(None);
        };
        let root = PathBuf::from(root)
            .canonicalize()
            .map_err(|e| e.to_string())?;
        let path = root.join("arm.json");
        if !path.exists() {
            return Ok(None);
        }
        let armed: Value = serde_json::from_slice(&fs::read(&path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        validate(&armed, tape, barcode)?;
        if armed["action"] == "fail" && std::env::var("REM_FIELD_ALLOW_FAIL").as_deref() != Ok("1")
        {
            return Err("field failure requires REM_FIELD_ALLOW_FAIL=1".into());
        }
        fs::rename(&path, root.join("consumed.json")).map_err(|e| e.to_string())?;
        Ok(Some(Self {
            root,
            tape,
            object: object.into(),
            armed,
            command_hash: None,
        }))
    }
    pub(super) fn dispatch(&mut self, bytes: &[u8]) {
        self.command_hash = Some(
            Sha256::digest(bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        );
    }
    /// Returns false only when the caller must refuse further dispatch.
    pub(super) fn settled(&mut self, bytes: u32) -> bool {
        let result = (|| -> Result<bool, Box<dyn std::error::Error>> {
            if bytes == 0 {
                return Ok(false);
            }
            let proof = json!({"schema":"rem.field.dispatch.v1", "wall_ns":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos().to_string(), "pid":std::process::id(),
                "tape_uuid":Uuid::from_bytes(self.tape).to_string(), "object_id":self.object,
                "nonce":self.armed["nonce"], "bytes_completed":bytes,
                "sha256": self.command_hash, "trace_offset": std::env::var_os("REM_FIELD_TRACE").and_then(|p| fs::metadata(p).ok()).map(|m|m.len()), "filemark":false, "committed":false});
            publish(&self.root, "holding", &proof)?;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.root.join("dispatch.tmp"))?;
            writeln!(file, "{proof}")?;
            file.sync_all()?;
            fs::rename(
                self.root.join("dispatch.tmp"),
                self.root.join("dispatch.json"),
            )?;
            fs::File::open(&self.root)?.sync_all()?;
            let started = Instant::now();
            let timeout = Duration::from_secs(60);
            #[cfg(test)]
            let timeout = std::env::var("REM_B8_TEST_HOLD_MS")
                .ok()
                .map(|v| Duration::from_millis(v.parse().expect("test hold duration")))
                .unwrap_or(timeout);
            while !self.root.join("release").exists() {
                if started.elapsed() >= timeout {
                    publish(&self.root, "timeout", &proof)?;
                    return Ok(false);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(self.armed["action"] == "hold")
        })();
        // Remove the live marker only after a settled exit has been recorded.
        let removed = fs::remove_file(self.root.join("holding.json"));
        result.unwrap_or(false) && removed.is_ok()
    }
}
/// Atomically publish durable field evidence before changing dispatch state.
fn publish(root: &std::path::Path, name: &str, value: &Value) -> std::io::Result<()> {
    let tmp = root.join(format!("{name}.tmp"));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    writeln!(file, "{value}")?;
    file.sync_all()?;
    fs::rename(tmp, root.join(format!("{name}.json")))?;
    fs::File::open(root)?.sync_all()
}
fn validate(arm: &Value, tape: [u8; 16], barcode: Option<&str>) -> Result<(), String> {
    if arm["schema"] != "rem.field.arm.v1"
        || arm["tape_uuid"] != Uuid::from_bytes(tape).to_string()
        || barcode.is_none()
        || arm["barcode"].as_str() != barcode
        || !["AOX032L9", "AOX036L9", "AOX038L9", "AOX058L9"]
            .contains(&arm["barcode"].as_str().unwrap_or(""))
        || !["hold", "fail"].contains(&arm["action"].as_str().unwrap_or(""))
        || arm["nonce"]
            .as_str()
            .and_then(|v| Uuid::parse_str(v).ok())
            .is_none_or(|u| u.is_nil())
    {
        return Err("invalid exact field arm identity/action".into());
    }
    Ok(())
}
/// Acknowledge actual ingress cancellation after the ring has been failed.
/// The paused actor may then resume into that failure, without a timing guess.
pub(crate) fn stream_cancelled(session: Uuid) {
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let Some(root) = std::env::var_os("REM_FIELD_GATE_DIR") else {
            return Ok(());
        };
        let root = PathBuf::from(root);
        let arm: Value = serde_json::from_slice(&fs::read(root.join("consumed.json"))?)?;
        if arm["session_id"] != session.to_string() || !root.join("dispatch.json").exists() {
            return Ok(());
        }
        let proof = json!({"session_id":session.to_string(), "nonce":arm["nonce"], "tape_uuid":arm["tape_uuid"], "pid":std::process::id()});
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join("cancelled.tmp"))?;
        writeln!(file, "{proof}")?;
        file.sync_all()?;
        fs::rename(root.join("cancelled.tmp"), root.join("cancelled.json"))?;
        Ok(())
    })();
    // Missing cancellation evidence is a hard timeout/refusal in the field helper.
    let _ = result;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arm_requires_allowlist_exact_tape_and_nonce() {
        let mut arm = json!({"schema":"rem.field.arm.v1", "tape_uuid":Uuid::from_bytes([1;16]).to_string(),
            "barcode":"AOX032L9", "action":"hold", "nonce":Uuid::new_v4().to_string()});
        assert!(validate(&arm, [1; 16], Some("AOX032L9")).is_ok());
        assert!(validate(&arm, [2; 16], Some("AOX032L9")).is_err());
        assert!(validate(&arm, [1; 16], None).is_err());
        arm["barcode"] = json!("AOX036L9");
        assert!(validate(&arm, [1; 16], Some("AOX032L9")).is_err());
        arm["barcode"] = json!("OTHER");
        assert!(validate(&arm, [1; 16], Some("AOX032L9")).is_err());
    }
}
