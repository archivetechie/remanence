//! Explicitly gated whole-Object crash cuts for system scenario WOR.
//!
//! This private adapter is inert unless the daemon is launched with the exact
//! opt-in value and an exact tape/object plan below the scenario-owned `/tmp`
//! root.  It records durable evidence after the planned number of physical
//! records has completed, then aborts before an Object filemark can be issued.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use remanence_library::{
    BlockSink, PipelinedWriteDiagnostics, TapeIoError, TapePosition, WriteBatchOutcome,
    WriteFilemarksOutcome, WriteOutcome,
};
use serde_json::{json, Value};
use uuid::Uuid;

pub(crate) const ENABLE_ENV: &str = "REM_WOR_OBJECT_FAULT_ENABLE";
pub(crate) const PLAN_ENV: &str = "REM_WOR_OBJECT_FAULT_PLAN";
const ENABLE_VALUE: &str = "whole-object-recovery-v1-abort";
const PLAN_SCHEMA: &str = "rem.wor.object-fault-plan.v1";
const EVIDENCE_SCHEMA: &str = "rem.wor.object-fault-evidence.v1";
const FAULT_ROOT: &str = "/tmp/system-harness/scenario-wor/object-faults";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ObjectFaultPlan {
    tape_uuid: [u8; 16],
    caller_object_id: String,
    nonce: Uuid,
    after_completed_records: u64,
    evidence_path: PathBuf,
}

impl ObjectFaultPlan {
    /// Load one exact WOR plan. Both environment variables absent is disabled.
    pub(crate) fn from_env_for_object(
        tape_uuid: [u8; 16],
        caller_object_id: &str,
    ) -> Result<Option<Self>, String> {
        Self::from_values_for_object(
            std::env::var_os(ENABLE_ENV),
            std::env::var_os(PLAN_ENV),
            tape_uuid,
            caller_object_id,
        )
    }

    fn from_values_for_object(
        enabled: Option<std::ffi::OsString>,
        plan_path: Option<std::ffi::OsString>,
        tape_uuid: [u8; 16],
        caller_object_id: &str,
    ) -> Result<Option<Self>, String> {
        if enabled.is_none() && plan_path.is_none() {
            return Ok(None);
        }
        let enabled = enabled.ok_or_else(|| {
            format!("{PLAN_ENV} is set but the explicit {ENABLE_ENV} opt-in is absent")
        })?;
        if enabled != OsStr::new(ENABLE_VALUE) {
            return Err(format!(
                "{ENABLE_ENV} must equal the exact WOR-only value {ENABLE_VALUE:?}"
            ));
        }
        let plan_path = PathBuf::from(plan_path.ok_or_else(|| {
            format!("{ENABLE_ENV} is set but the exact {PLAN_ENV} path is absent")
        })?);
        require_confined_path(&plan_path, "fault plan")?;
        require_existing_path_confined(&plan_path, "fault plan")?;
        let bytes = fs::read(&plan_path).map_err(|error| {
            format!(
                "read WOR object fault plan {}: {error}",
                plan_path.display()
            )
        })?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
            format!(
                "decode WOR object fault plan {}: {error}",
                plan_path.display()
            )
        })?;
        let plan = Self::parse(&value)?;
        if plan.tape_uuid != tape_uuid || plan.caller_object_id != caller_object_id {
            return Err(
                "WOR object fault plan does not match the exact tape and caller object id"
                    .to_string(),
            );
        }
        if plan.evidence_path.exists() {
            plan.validate_existing_evidence()?;
        }
        Ok(Some(plan))
    }

    fn parse(value: &Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or_else(|| "WOR object fault plan must be a JSON object".to_string())?;
        let required = [
            "schema",
            "tape_uuid",
            "caller_object_id",
            "nonce",
            "after_completed_records",
            "evidence_path",
        ];
        if object.len() != required.len() || required.iter().any(|key| !object.contains_key(*key)) {
            return Err(format!(
                "WOR object fault plan must contain exactly {}",
                required.join(", ")
            ));
        }
        require_text(object, "schema", PLAN_SCHEMA)?;
        let tape_uuid_text = text_field(object, "tape_uuid")?;
        let tape_uuid = Uuid::parse_str(tape_uuid_text)
            .map_err(|error| format!("invalid WOR object fault tape_uuid: {error}"))?;
        if tape_uuid.to_string() != tape_uuid_text {
            return Err("WOR object fault tape_uuid must be canonical lowercase UUID text".into());
        }
        let caller_object_id = text_field(object, "caller_object_id")?.to_string();
        let nonce_text = text_field(object, "nonce")?;
        let nonce = Uuid::parse_str(nonce_text)
            .map_err(|error| format!("invalid WOR object fault nonce: {error}"))?;
        if nonce.is_nil() || nonce.to_string() != nonce_text {
            return Err("WOR object fault nonce must be a non-nil canonical UUID".into());
        }
        let after_completed_records = object
            .get("after_completed_records")
            .and_then(Value::as_u64)
            .filter(|count| *count != 0)
            .ok_or_else(|| {
                "WOR object fault after_completed_records must be a nonzero JSON u64".to_string()
            })?;
        let evidence_path = PathBuf::from(text_field(object, "evidence_path")?);
        require_confined_path(&evidence_path, "evidence")?;
        Ok(Self {
            tape_uuid: *tape_uuid.as_bytes(),
            caller_object_id,
            nonce,
            after_completed_records,
            evidence_path,
        })
    }

    fn record_after_write(
        &self,
        completed_records: u64,
        position_after: TapePosition,
    ) -> Result<bool, String> {
        if self.evidence_path.exists() {
            self.validate_existing_evidence()?;
            return Ok(false);
        }
        if completed_records < self.after_completed_records {
            return Ok(false);
        }
        let evidence = json!({
            "schema": EVIDENCE_SCHEMA,
            "tape_uuid": Uuid::from_bytes(self.tape_uuid).to_string(),
            "caller_object_id": self.caller_object_id,
            "nonce": self.nonce.to_string(),
            "after_completed_records": self.after_completed_records,
            "completed_records": completed_records,
            "position_after": {
                "partition": position_after.partition,
                "lba": position_after.lba.to_string(),
            },
        });
        match write_durable_new_json(&self.evidence_path, &evidence)? {
            PublishOutcome::Published => Ok(true),
            PublishOutcome::AlreadyExists => {
                self.validate_existing_evidence()?;
                Ok(false)
            }
        }
    }

    fn cut_is_pending(&self) -> Result<bool, String> {
        if self.evidence_path.exists() {
            self.validate_existing_evidence()?;
            return Ok(false);
        }
        // A reached threshold without matching durable evidence is never a
        // reason to allow the filemark. In production the data call that
        // reached it either records+aborts or returns an evidence error.
        Ok(true)
    }

    fn validate_existing_evidence(&self) -> Result<(), String> {
        require_existing_path_confined(&self.evidence_path, "evidence")?;
        let bytes = fs::read(&self.evidence_path).map_err(|error| {
            format!(
                "read existing WOR object fault evidence {}: {error}",
                self.evidence_path.display()
            )
        })?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
            format!(
                "decode existing WOR object fault evidence {}: {error}",
                self.evidence_path.display()
            )
        })?;
        let object = value.as_object().ok_or_else(|| {
            format!(
                "existing WOR object fault evidence {} is not a JSON object",
                self.evidence_path.display()
            )
        })?;
        let required = [
            "schema",
            "tape_uuid",
            "caller_object_id",
            "nonce",
            "after_completed_records",
            "completed_records",
            "position_after",
        ];
        if object.len() != required.len() || required.iter().any(|key| !object.contains_key(*key)) {
            return Err(format!(
                "existing WOR object fault evidence {} has an unexpected schema shape",
                self.evidence_path.display()
            ));
        }
        for (field, expected) in [
            ("schema", EVIDENCE_SCHEMA.to_string()),
            ("tape_uuid", Uuid::from_bytes(self.tape_uuid).to_string()),
            ("caller_object_id", self.caller_object_id.clone()),
            ("nonce", self.nonce.to_string()),
        ] {
            if object.get(field).and_then(Value::as_str) != Some(expected.as_str()) {
                return Err(format!(
                    "existing WOR object fault evidence {} has mismatched {field}",
                    self.evidence_path.display()
                ));
            }
        }
        if object
            .get("after_completed_records")
            .and_then(Value::as_u64)
            != Some(self.after_completed_records)
        {
            return Err(format!(
                "existing WOR object fault evidence {} has mismatched after_completed_records",
                self.evidence_path.display()
            ));
        }
        if object
            .get("completed_records")
            .and_then(Value::as_u64)
            .is_none_or(|count| count < self.after_completed_records)
        {
            return Err(format!(
                "existing WOR object fault evidence {} has invalid completed_records",
                self.evidence_path.display()
            ));
        }
        validate_evidence_position(object.get("position_after"), &self.evidence_path)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AbortBehavior {
    AbortProcess,
    #[cfg(test)]
    RecordOnly,
}

/// A transparent production `BlockSink` adapter which cuts only after a
/// successful data command and never permits an armed plan to miss at filemark.
pub(crate) struct ObjectFaultSink<'a, S: BlockSink + ?Sized> {
    inner: &'a mut S,
    fault: Option<&'a ObjectFaultPlan>,
    completed_records: u64,
    abort_behavior: AbortBehavior,
    #[cfg(test)]
    abort_requested: bool,
}

impl<'a, S: BlockSink + ?Sized> ObjectFaultSink<'a, S> {
    pub(crate) fn new(inner: &'a mut S, fault: Option<&'a ObjectFaultPlan>) -> Self {
        Self {
            inner,
            fault,
            completed_records: 0,
            abort_behavior: AbortBehavior::AbortProcess,
            #[cfg(test)]
            abort_requested: false,
        }
    }

    #[cfg(test)]
    fn new_record_only(inner: &'a mut S, fault: Option<&'a ObjectFaultPlan>) -> Self {
        Self {
            inner,
            fault,
            completed_records: 0,
            abort_behavior: AbortBehavior::RecordOnly,
            abort_requested: false,
        }
    }

    fn account_completed(
        &mut self,
        records: u64,
        position_after: TapePosition,
    ) -> Result<(), TapeIoError> {
        self.completed_records = self.completed_records.checked_add(records).ok_or_else(|| {
            TapeIoError::OperationFailed("WOR object fault record counter overflow".into())
        })?;
        let Some(fault) = self.fault else {
            return Ok(());
        };
        let triggered = fault
            .record_after_write(self.completed_records, position_after)
            .map_err(TapeIoError::OperationFailed)?;
        if triggered {
            match self.abort_behavior {
                AbortBehavior::AbortProcess => std::process::abort(),
                #[cfg(test)]
                AbortBehavior::RecordOnly => self.abort_requested = true,
            }
        }
        Ok(())
    }

    fn reject_early_filemark(&self, count: u32) -> Result<(), TapeIoError> {
        if count == 0 {
            return Ok(());
        }
        let Some(fault) = self.fault else {
            return Ok(());
        };
        if fault
            .cut_is_pending()
            .map_err(TapeIoError::OperationFailed)?
        {
            return Err(TapeIoError::OperationFailed(format!(
                "WOR object filemark reached after {} completed records before planned cut {}",
                self.completed_records, fault.after_completed_records
            )));
        }
        Ok(())
    }
}

impl<S: BlockSink + ?Sized> BlockSink for ObjectFaultSink<'_, S> {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        let outcome = self.inner.write_block(buf)?;
        let completed =
            u64::from(!buf.is_empty() && usize::try_from(outcome.bytes_written) == Ok(buf.len()));
        self.account_completed(completed, outcome.position_after)?;
        Ok(outcome)
    }

    fn write_block_batch(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        let outcome = self.inner.write_block_batch(buf, block_size_bytes)?;
        self.account_completed(u64::from(outcome.records_written), outcome.position_after)?;
        Ok(outcome)
    }

    fn write_block_batch_pipelined(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
        cdb: &[u8],
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        let outcome = self
            .inner
            .write_block_batch_pipelined(buf, block_size_bytes, cdb)?;
        self.account_completed(u64::from(outcome.records_written), outcome.position_after)?;
        Ok(outcome)
    }

    fn write_batch_blocks(&self, block_size_bytes: u32) -> u32 {
        self.inner.write_batch_blocks(block_size_bytes)
    }

    fn requested_write_batch_blocks(&self) -> u32 {
        self.inner.requested_write_batch_blocks()
    }

    fn staging_ring_buffers(&self) -> u32 {
        self.inner.staging_ring_buffers()
    }

    fn pipelined_write_diagnostics(&self) -> PipelinedWriteDiagnostics {
        self.inner.pipelined_write_diagnostics()
    }

    fn reset_pipelined_write_diagnostics(&mut self) {
        self.inner.reset_pipelined_write_diagnostics();
    }

    fn publish_pipelined_write_diagnostics(&mut self) {
        self.inner.publish_pipelined_write_diagnostics();
    }

    fn begin_pipelined_write_window(
        &mut self,
        command_count: u32,
        bytes: u64,
        first_records: u32,
        last_records: u32,
    ) {
        self.inner
            .begin_pipelined_write_window(command_count, bytes, first_records, last_records);
    }

    fn finish_pipelined_write_window_success(
        &mut self,
        command_count: u32,
        bytes: u64,
        first_records: u32,
        last_records: u32,
        duration: Duration,
    ) {
        self.inner.finish_pipelined_write_window_success(
            command_count,
            bytes,
            first_records,
            last_records,
            duration,
        );
    }

    fn finish_pipelined_write_window_error(
        &mut self,
        command_count: u32,
        bytes: u64,
        first_records: u32,
        last_records: u32,
        error: &TapeIoError,
    ) {
        self.inner.finish_pipelined_write_window_error(
            command_count,
            bytes,
            first_records,
            last_records,
            error,
        );
    }

    fn flush_pending_pipeline_audit(&mut self) {
        self.inner.flush_pending_pipeline_audit();
    }

    fn position_check_bytes(&self) -> u64 {
        self.inner.position_check_bytes()
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.reject_early_filemark(count)?;
        self.inner.write_filemarks(count)
    }

    fn write_filemarks_immediate(&mut self, count: u32) -> Result<(), TapeIoError> {
        self.reject_early_filemark(count)?;
        self.inner.write_filemarks_immediate(count)
    }

    fn write_filemarks_pipelined(
        &mut self,
        count: u32,
    ) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.reject_early_filemark(count)?;
        self.inner.write_filemarks_pipelined(count)
    }

    fn space_to_end_of_data(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.space_to_end_of_data()
    }

    fn space_to_end_of_data_pipelined(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.space_to_end_of_data_pipelined()
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        self.inner.locate(lba)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.position()
    }

    fn position_pipelined(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.position_pipelined()
    }
}

fn text_field<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
) -> Result<&'a str, String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("WOR object fault plan field {field:?} must be nonempty text"))
}

fn require_text(
    object: &serde_json::Map<String, Value>,
    field: &str,
    expected: &str,
) -> Result<(), String> {
    let actual = text_field(object, field)?;
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "WOR object fault plan field {field:?} must equal {expected:?}, got {actual:?}"
        ))
    }
}

fn require_confined_path(path: &Path, label: &str) -> Result<(), String> {
    let root = Path::new(FAULT_ROOT);
    let clean_components = path
        .components()
        .all(|component| matches!(component, Component::RootDir | Component::Normal(_)));
    if !path.is_absolute() || path == root || !path.starts_with(root) || !clean_components {
        return Err(format!(
            "WOR object fault {label} path {} must be absolute and confined beneath {FAULT_ROOT}",
            path.display()
        ));
    }
    Ok(())
}

fn require_existing_path_confined(path: &Path, label: &str) -> Result<(), String> {
    require_confined_path(path, label)?;
    let root = fs::canonicalize(FAULT_ROOT)
        .map_err(|error| format!("resolve WOR object fault root {FAULT_ROOT}: {error}"))?;
    let resolved = fs::canonicalize(path).map_err(|error| {
        format!(
            "resolve WOR object fault {label} {}: {error}",
            path.display()
        )
    })?;
    if !resolved.starts_with(&root) || resolved == root {
        return Err(format!(
            "resolved WOR object fault {label} path {} escapes {FAULT_ROOT}",
            path.display()
        ));
    }
    Ok(())
}

fn require_parent_confined(path: &Path) -> Result<&Path, String> {
    require_confined_path(path, "evidence")?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("WOR object fault evidence {} has no parent", path.display()))?;
    let root = fs::canonicalize(FAULT_ROOT)
        .map_err(|error| format!("resolve WOR object fault root {FAULT_ROOT}: {error}"))?;
    let mut existing_ancestor = parent;
    while !existing_ancestor.exists() {
        existing_ancestor = existing_ancestor.parent().ok_or_else(|| {
            format!(
                "WOR object fault evidence path {} has no existing ancestor",
                path.display()
            )
        })?;
    }
    let resolved_ancestor = fs::canonicalize(existing_ancestor).map_err(|error| {
        format!(
            "resolve WOR object fault evidence ancestor {}: {error}",
            existing_ancestor.display()
        )
    })?;
    if !resolved_ancestor.starts_with(&root) {
        return Err(format!(
            "resolved WOR object fault evidence path {} escapes {FAULT_ROOT}",
            path.display()
        ));
    }
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "create WOR object fault evidence directory {}: {error}",
            parent.display()
        )
    })?;
    let resolved_parent = fs::canonicalize(parent).map_err(|error| {
        format!(
            "resolve WOR object fault evidence directory {}: {error}",
            parent.display()
        )
    })?;
    if !resolved_parent.starts_with(root) {
        return Err(format!(
            "resolved WOR object fault evidence path {} escapes {FAULT_ROOT}",
            path.display()
        ));
    }
    Ok(parent)
}

fn validate_evidence_position(value: Option<&Value>, path: &Path) -> Result<(), String> {
    let Some(position) = value.and_then(Value::as_object) else {
        return Err(format!(
            "existing WOR object fault evidence {} has invalid position_after",
            path.display()
        ));
    };
    if position.len() != 2
        || position
            .get("partition")
            .and_then(Value::as_u64)
            .is_none_or(|partition| partition > u64::from(u32::MAX))
        || position
            .get("lba")
            .and_then(Value::as_str)
            .and_then(|text| text.parse::<u64>().ok().map(|lba| (text, lba)))
            .filter(|(text, lba)| **text == lba.to_string())
            .is_none()
    {
        return Err(format!(
            "existing WOR object fault evidence {} has invalid position_after",
            path.display()
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PublishOutcome {
    Published,
    AlreadyExists,
}

fn write_durable_new_json(path: &Path, value: &Value) -> Result<PublishOutcome, String> {
    let parent = require_parent_confined(path)?;
    let file_name = path.file_name().ok_or_else(|| {
        format!(
            "WOR object fault evidence {} has no file name",
            path.display()
        )
    })?;
    let temporary = parent.join(format!(
        ".{}.{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id(),
        Uuid::new_v4()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        serde_json::to_writer_pretty(&mut file, value).map_err(io::Error::other)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        match publish_noreplace(&temporary, path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                return Ok(PublishOutcome::AlreadyExists)
            }
            Err(error) => return Err(error),
        }
        File::open(parent)?.sync_all()?;
        Ok(PublishOutcome::Published)
    })();
    if result.is_err() || temporary.exists() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|error| {
        format!(
            "publish durable WOR object fault evidence {}: {error}",
            path.display()
        )
    })
}

/// Atomically publishes `source` without replacing an existing destination.
fn publish_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let source = CString::new(source.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "source path contains NUL"))?;
        let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
            io::Error::new(ErrorKind::InvalidInput, "destination path contains NUL")
        })?;
        // SAFETY: both pointers remain valid NUL-terminated strings for the syscall.
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                destination.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if !matches!(
            error.raw_os_error(),
            Some(libc::ENOSYS) | Some(libc::EINVAL) | Some(libc::EOPNOTSUPP)
        ) {
            return Err(error);
        }
    }

    fs::hard_link(source, destination)?;
    fs::remove_file(source)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir() -> PathBuf {
        let path = Path::new(FAULT_ROOT).join(format!("unit-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn plan_value(evidence_path: &Path) -> Value {
        json!({
            "schema": PLAN_SCHEMA,
            "tape_uuid": "71717171-7171-7171-7171-717171717171",
            "caller_object_id": "caller-object-71",
            "nonce": "81818181-8181-8181-8181-818181818181",
            "after_completed_records": 3,
            "evidence_path": evidence_path,
        })
    }

    fn position(lba: u64) -> TapePosition {
        TapePosition {
            lba,
            partition: 0,
            beginning_of_partition: lba == 0,
            end_of_partition: false,
            block_position_end_of_warning: false,
        }
    }

    #[derive(Default)]
    struct MockSink {
        writes: u32,
        batches: u32,
        pipelined_batches: u32,
        filemarks: u32,
        callbacks: Vec<&'static str>,
        fail_write: bool,
    }

    impl BlockSink for MockSink {
        fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
            if self.fail_write {
                return Err(TapeIoError::OperationFailed(
                    "injected write failure".into(),
                ));
            }
            self.writes += 1;
            Ok(WriteOutcome::from_computed_position(
                buf.len() as u32,
                false,
                false,
                position(u64::from(self.writes)),
            ))
        }

        fn write_block_batch(
            &mut self,
            buf: &[u8],
            block_size_bytes: u32,
        ) -> Result<WriteBatchOutcome, TapeIoError> {
            self.batches += 1;
            let records = (buf.len() / block_size_bytes as usize) as u32;
            Ok(WriteBatchOutcome::from_computed_position(
                records,
                buf.len() as u32,
                false,
                false,
                position(u64::from(records)),
            ))
        }

        fn write_block_batch_pipelined(
            &mut self,
            buf: &[u8],
            block_size_bytes: u32,
            _cdb: &[u8],
        ) -> Result<WriteBatchOutcome, TapeIoError> {
            self.pipelined_batches += 1;
            let records = (buf.len() / block_size_bytes as usize) as u32;
            Ok(WriteBatchOutcome::from_computed_position(
                records,
                buf.len() as u32,
                false,
                false,
                position(u64::from(records) + 10),
            ))
        }

        fn write_batch_blocks(&self, _: u32) -> u32 {
            7
        }

        fn requested_write_batch_blocks(&self) -> u32 {
            8
        }

        fn staging_ring_buffers(&self) -> u32 {
            9
        }

        fn reset_pipelined_write_diagnostics(&mut self) {
            self.callbacks.push("reset");
        }

        fn publish_pipelined_write_diagnostics(&mut self) {
            self.callbacks.push("publish");
        }

        fn flush_pending_pipeline_audit(&mut self) {
            self.callbacks.push("flush");
        }

        fn begin_pipelined_write_window(&mut self, _: u32, _: u64, _: u32, _: u32) {
            self.callbacks.push("begin-window");
        }

        fn finish_pipelined_write_window_success(
            &mut self,
            _: u32,
            _: u64,
            _: u32,
            _: u32,
            _: Duration,
        ) {
            self.callbacks.push("finish-window-success");
        }

        fn finish_pipelined_write_window_error(
            &mut self,
            _: u32,
            _: u64,
            _: u32,
            _: u32,
            _: &TapeIoError,
        ) {
            self.callbacks.push("finish-window-error");
        }

        fn position_check_bytes(&self) -> u64 {
            1234
        }

        fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
            self.filemarks += count;
            Ok(WriteFilemarksOutcome::from_computed_position(position(99)))
        }

        fn space_to_end_of_data(&mut self) -> Result<TapePosition, TapeIoError> {
            self.callbacks.push("space");
            Ok(position(50))
        }

        fn space_to_end_of_data_pipelined(&mut self) -> Result<TapePosition, TapeIoError> {
            self.callbacks.push("space-pipelined");
            Ok(position(51))
        }

        fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
            self.callbacks.push("locate");
            Ok(position(lba))
        }

        fn position(&mut self) -> Result<TapePosition, TapeIoError> {
            self.callbacks.push("position");
            Ok(position(52))
        }

        fn position_pipelined(&mut self) -> Result<TapePosition, TapeIoError> {
            self.callbacks.push("position-pipelined");
            Ok(position(53))
        }
    }

    #[test]
    fn parser_rejects_hostile_or_non_exact_plans() {
        let directory = test_dir();
        let valid = plan_value(&directory.join("evidence.json"));
        assert!(ObjectFaultPlan::parse(&valid).is_ok());

        let mut vectors = Vec::new();
        let mut extra = valid.clone();
        extra
            .as_object_mut()
            .unwrap()
            .insert("extra".into(), json!(1));
        vectors.push(extra);
        for (field, replacement) in [
            ("nonce", json!(Uuid::nil().to_string())),
            ("after_completed_records", json!(0)),
            ("after_completed_records", json!("3")),
            ("caller_object_id", json!("")),
            ("evidence_path", json!("relative/evidence.json")),
            (
                "evidence_path",
                json!("/tmp/system-harness/scenario-wor/object-faults-escape/evidence.json"),
            ),
            (
                "evidence_path",
                json!(format!("{FAULT_ROOT}/inside/../outside.json")),
            ),
            ("tape_uuid", json!("71717171-7171-7171-7171-71717171717A")),
        ] {
            let mut value = valid.clone();
            value
                .as_object_mut()
                .unwrap()
                .insert(field.into(), replacement);
            vectors.push(value);
        }
        for vector in vectors {
            assert!(
                ObjectFaultPlan::parse(&vector).is_err(),
                "accepted {vector}"
            );
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn environment_gate_is_default_off_exact_and_object_scoped() {
        let directory = test_dir();
        let evidence_path = directory.join("evidence.json");
        let plan_path = directory.join("plan.json");
        fs::write(
            &plan_path,
            serde_json::to_vec(&plan_value(&evidence_path)).unwrap(),
        )
        .unwrap();
        let tape_uuid = *Uuid::parse_str("71717171-7171-7171-7171-717171717171")
            .unwrap()
            .as_bytes();

        assert_eq!(
            ObjectFaultPlan::from_values_for_object(None, None, tape_uuid, "caller-object-71")
                .unwrap(),
            None
        );
        for (enabled, path) in [
            (None, Some(plan_path.as_os_str().to_owned())),
            (Some(ENABLE_VALUE.into()), None),
            (
                Some("almost-enabled".into()),
                Some(plan_path.as_os_str().to_owned()),
            ),
        ] {
            assert!(ObjectFaultPlan::from_values_for_object(
                enabled,
                path,
                tape_uuid,
                "caller-object-71"
            )
            .is_err());
        }
        assert!(ObjectFaultPlan::from_values_for_object(
            Some(ENABLE_VALUE.into()),
            Some(plan_path.as_os_str().to_owned()),
            [0x72; 16],
            "caller-object-71"
        )
        .is_err());
        assert!(ObjectFaultPlan::from_values_for_object(
            Some(ENABLE_VALUE.into()),
            Some(plan_path.as_os_str().to_owned()),
            tape_uuid,
            "different-caller"
        )
        .is_err());
        assert!(ObjectFaultPlan::from_values_for_object(
            Some(ENABLE_VALUE.into()),
            Some(plan_path.into_os_string()),
            tape_uuid,
            "caller-object-71"
        )
        .unwrap()
        .is_some());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn single_block_cut_occurs_only_after_successful_threshold_write() {
        let directory = test_dir();
        let evidence_path = directory.join("evidence.json");
        let plan = ObjectFaultPlan::parse(&plan_value(&evidence_path)).unwrap();
        let mut inner = MockSink::default();
        let mut sink = ObjectFaultSink::new_record_only(&mut inner, Some(&plan));

        sink.write_block(&[0u8; 4]).unwrap();
        sink.write_block(&[0u8; 4]).unwrap();
        assert!(!sink.abort_requested);
        assert!(!evidence_path.exists());
        sink.write_block(&[0u8; 4]).unwrap();
        assert!(sink.abort_requested);
        assert_eq!(sink.completed_records, 3);
        assert_eq!(inner.writes, 3);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn successful_batch_records_exact_evidence_without_aborting_test() {
        let directory = test_dir();
        let evidence_path = directory.join("evidence.json");
        let plan = ObjectFaultPlan::parse(&plan_value(&evidence_path)).unwrap();
        let mut inner = MockSink::default();
        let mut sink = ObjectFaultSink::new_record_only(&mut inner, Some(&plan));

        let outcome = sink.write_block_batch(&[0u8; 16], 4).unwrap();
        assert_eq!(outcome.records_written, 4);
        assert!(sink.abort_requested);
        assert_eq!(inner.batches, 1);

        let evidence: Value = serde_json::from_slice(&fs::read(&evidence_path).unwrap()).unwrap();
        assert_eq!(evidence["completed_records"], 4);
        assert_eq!(evidence["after_completed_records"], 3);
        assert_eq!(evidence["position_after"]["lba"], "4");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn pipelined_batch_is_counted_and_matching_evidence_is_one_shot() {
        let directory = test_dir();
        let evidence_path = directory.join("evidence.json");
        let plan = ObjectFaultPlan::parse(&plan_value(&evidence_path)).unwrap();
        let mut first = MockSink::default();
        let mut sink = ObjectFaultSink::new_record_only(&mut first, Some(&plan));
        sink.write_block_batch_pipelined(&[0u8; 12], 4, &[1, 2])
            .unwrap();
        assert!(sink.abort_requested);
        let mut resumed = MockSink::default();
        let mut resumed_sink = ObjectFaultSink::new_record_only(&mut resumed, Some(&plan));
        resumed_sink.write_block(&[0u8; 4]).unwrap();
        assert!(!resumed_sink.abort_requested);
        resumed_sink.write_filemarks(1).unwrap();
        assert_eq!(resumed.filemarks, 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_write_does_not_count_and_early_filemark_fails_closed() {
        let directory = test_dir();
        let evidence_path = directory.join("evidence.json");
        let plan = ObjectFaultPlan::parse(&plan_value(&evidence_path)).unwrap();
        let mut inner = MockSink {
            fail_write: true,
            ..MockSink::default()
        };
        let mut sink = ObjectFaultSink::new_record_only(&mut inner, Some(&plan));
        assert!(sink.write_block(&[0u8; 4]).is_err());
        assert!(sink.write_filemarks(0).is_ok());
        assert!(sink.write_filemarks(1).is_err());
        assert!(sink.write_filemarks_immediate(1).is_err());
        assert!(sink.write_filemarks_pipelined(1).is_err());
        assert_eq!(sink.completed_records, 0);
        assert_eq!(inner.filemarks, 0);
        assert!(!evidence_path.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn conflicting_existing_evidence_fails_closed() {
        let directory = test_dir();
        let evidence_path = directory.join("evidence.json");
        let plan = ObjectFaultPlan::parse(&plan_value(&evidence_path)).unwrap();
        fs::write(&evidence_path, br#"{"schema":"foreign"}"#).unwrap();
        let mut inner = MockSink::default();
        let mut sink = ObjectFaultSink::new_record_only(&mut inner, Some(&plan));
        let error = sink.write_block(&[0u8; 4]).unwrap_err();
        assert!(error
            .to_string()
            .contains("existing WOR object fault evidence"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn disabled_adapter_delegates_tuning_and_position_surfaces() {
        let mut inner = MockSink::default();
        let mut sink = ObjectFaultSink::new_record_only(&mut inner, None);
        assert_eq!(sink.write_batch_blocks(4), 7);
        assert_eq!(sink.requested_write_batch_blocks(), 8);
        assert_eq!(sink.staging_ring_buffers(), 9);
        assert_eq!(sink.position_check_bytes(), 1234);
        sink.reset_pipelined_write_diagnostics();
        sink.publish_pipelined_write_diagnostics();
        sink.flush_pending_pipeline_audit();
        sink.begin_pipelined_write_window(2, 8, 1, 1);
        sink.finish_pipelined_write_window_success(2, 8, 1, 1, Duration::ZERO);
        sink.finish_pipelined_write_window_error(
            2,
            8,
            1,
            1,
            &TapeIoError::OperationFailed("test".into()),
        );
        assert_eq!(sink.space_to_end_of_data().unwrap().lba, 50);
        assert_eq!(sink.space_to_end_of_data_pipelined().unwrap().lba, 51);
        assert_eq!(sink.locate(77).unwrap().lba, 77);
        assert_eq!(sink.position().unwrap().lba, 52);
        assert_eq!(sink.position_pipelined().unwrap().lba, 53);
        assert_eq!(
            inner.callbacks,
            [
                "reset",
                "publish",
                "flush",
                "begin-window",
                "finish-window-success",
                "finish-window-error",
                "space",
                "space-pipelined",
                "locate",
                "position",
                "position-pipelined"
            ]
        );
    }
}
