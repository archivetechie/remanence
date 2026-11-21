//! Opt-in, process-wide evidence at the Linux SG_IO funnel.
//!
//! Transfer payloads never enter this recorder. Only standard INQUIRY identity
//! fields and raw error sense are retained. Rows and cumulative runs share one
//! caller-thread lock, released before the ioctl. Every transport boundary
//! contains recorder panics; a caught panic disables tracing for the process.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use remanence_scsi::{inquiry::Inquiry, ScsiError};
use serde_json::{json, Value};

const RUN_INTERVAL_NS: u64 = 10_000_000_000;
static PROCESS_TRACE: OnceLock<Option<Arc<Recorder>>> = OnceLock::new();
static DISABLED: AtomicBool = AtomicBool::new(false);
static PANIC_REPORTED: AtomicBool = AtomicBool::new(false);

// Declared after the mutex guard, so unwinding disables before unlocking.
struct DisableOnUnwind {
    completed: bool,
}
impl Drop for DisableOnUnwind {
    fn drop(&mut self) {
        if !self.completed {
            DISABLED.store(true, Ordering::Release);
        }
    }
}

fn diagnostic(message: &str) {
    let _ = writeln!(std::io::stderr(), "{message}");
}

fn guarded<T>(work: impl FnOnce() -> T) -> Option<T> {
    if DISABLED.load(Ordering::Acquire) {
        return None;
    }
    match catch_unwind(AssertUnwindSafe(work)) {
        Ok(result) => Some(result),
        Err(payload) => {
            // Even a panic payload can have a panicking destructor. Retain it
            // rather than unwinding from recovery; this happens once per process.
            std::mem::forget(payload);
            DISABLED.store(true, Ordering::Release);
            if !PANIC_REPORTED.swap(true, Ordering::AcqRel) {
                diagnostic("rem: command trace disabled: recorder panicked");
            }
            None
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Policy<'a> {
    Off,
    FieldTrace,
    Directory(&'a Path),
}

fn policy<'a>(trace: Option<&'a OsStr>, field_trace: Option<&OsStr>) -> Policy<'a> {
    match trace.filter(|value| !value.is_empty()) {
        None => Policy::Off,
        Some(_) if field_trace.is_some() => Policy::FieldTrace,
        Some(value) => Policy::Directory(Path::new(value)),
    }
}

/// Called only after the device fd is open. Includes setup in the panic boundary.
pub(super) fn setup() -> (Option<Arc<Recorder>>, u64) {
    guarded(|| {
        let trace = PROCESS_TRACE
            .get_or_init(|| {
                let trace = std::env::var_os("REM_TRACE");
                let field = std::env::var_os("REM_FIELD_TRACE");
                match policy(trace.as_deref(), field.as_deref()) {
                    Policy::Off => None,
                    Policy::FieldTrace => {
                        diagnostic("rem: command trace disabled while the field trace is on");
                        None
                    }
                    Policy::Directory(directory) => open_recorder(
                        directory,
                        std::process::id(),
                        SystemTime::now(),
                        &mut diagnostic,
                    ),
                }
            })
            .clone();
        let dev = trace.as_ref().map_or(0, |recorder| recorder.device());
        (trace, dev)
    })
    .unwrap_or_default()
}

fn open_recorder(
    directory: &Path,
    pid: u32,
    wall: SystemTime,
    report: &mut dyn FnMut(&str),
) -> Option<Arc<Recorder>> {
    let opened = (|| {
        if !directory.metadata()?.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                "command trace location is not a directory",
            ));
        }
        let seconds = wall
            .duration_since(UNIX_EPOCH)
            .map_err(std::io::Error::other)?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(directory.join(format!("rem-trace-{pid}-{}.jsonl", seconds.as_secs())))?;
        Ok((file, file_soft_limit()?))
    })();
    match opened {
        Ok((file, limit)) => Some(Arc::new(Recorder::new(file, pid, wall, limit))),
        Err(error) => {
            report(&format!("rem: command trace disabled: {error}"));
            None
        }
    }
}

fn file_soft_limit() -> std::io::Result<Option<u64>> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes only to this live rlimit value.
    if unsafe { libc::getrlimit(libc::RLIMIT_FSIZE, &mut limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((limit.rlim_cur != libc::RLIM_INFINITY).then_some(limit.rlim_cur))
}

/// The file and test doubles obey the same write/sync interface.
trait Writer: Send {
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()>;
    fn sync_data(&mut self) -> std::io::Result<()>;
}

impl Writer for File {
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        Write::write_all(self, bytes)
    }
    fn sync_data(&mut self) -> std::io::Result<()> {
        File::sync_data(self)
    }
}

#[cfg(test)]
type DiagnosticHook = Arc<dyn Fn(&str, bool) + Send + Sync>;

pub(super) struct Recorder {
    state: Mutex<State>,
    epoch: Instant,
    #[cfg(test)]
    clock: Option<Arc<std::sync::atomic::AtomicU64>>,
    #[cfg(test)]
    lock_waiting: Option<Arc<std::sync::Barrier>>,
    #[cfg(test)]
    diagnostic_hook: Option<DiagnosticHook>,
}

struct State {
    writer: Box<dyn Writer>,
    pending_header: Option<Value>,
    needs_newline: bool,
    devices: HashMap<u64, Device>,
    next_device: u64,
    warned: bool,
    pending_diagnostics: Vec<String>,
    file_limit: Option<u64>,
    // Reserve the entire attempted write, including on an error: write_all
    // cannot tell us how many bytes a partial failure left in the file.
    reserved_bytes: u64,
    #[cfg(test)]
    sync_count: usize,
    #[cfg(test)]
    warning_count: usize,
}

#[derive(Default)]
struct Device {
    identified: bool,
    run: Option<Run>,
}

#[derive(PartialEq, Eq)]
struct Key {
    cdb: Option<Vec<u8>>,
    requested_bytes: usize,
    timeout_ms: u32,
}

struct Run {
    key: Key,
    count: u64,
    bytes: u64,
    total_ns: u64,
    max_ns: u64,
    first_mono_ns: u64,
    last_mono_ns: u64,
    last_row_ns: u64,
}

pub(super) struct Command {
    dev: u64,
    key: Key,
    opcode: Option<u8>,
    dir: Direction,
    start_mono_ns: u64,
    dispatch_mono_ns: u64,
    matching_run: bool,
}

#[derive(Clone, Copy)]
pub(super) enum Direction {
    In,
    None,
    Out,
}
impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Self::In => "in",
            Self::None => "none",
            Self::Out => "out",
        }
    }
}

impl Recorder {
    fn new(file: File, pid: u32, wall: SystemTime, limit: Option<u64>) -> Self {
        Self::new_writer_with_limit(Box::new(file), pid, wall, limit)
    }

    #[cfg(test)]
    fn new_writer(writer: Box<dyn Writer>, pid: u32, wall: SystemTime) -> Self {
        Self::new_writer_with_limit(writer, pid, wall, None)
    }

    fn new_writer_with_limit(
        writer: Box<dyn Writer>,
        pid: u32,
        wall: SystemTime,
        limit: Option<u64>,
    ) -> Self {
        let recorder = Self {
            state: Mutex::new(State {
                writer,
                pending_header: None,
                needs_newline: false,
                devices: HashMap::new(),
                next_device: 1,
                warned: false,
                pending_diagnostics: Vec::new(),
                file_limit: limit,
                reserved_bytes: 0,
                #[cfg(test)]
                sync_count: 0,
                #[cfg(test)]
                warning_count: 0,
            }),
            epoch: Instant::now(),
            #[cfg(test)]
            clock: None,
            #[cfg(test)]
            lock_waiting: None,
            #[cfg(test)]
            diagnostic_hook: None,
        };
        let header = json!({"event":"header", "mono_ns":recorder.now(),
            "schema":1, "rem_version":env!("CARGO_PKG_VERSION"), "pid":pid, "wall":wall_utc(wall)});
        let _ = recorder.with_state(|state| {
            state.pending_header = Some(header);
            state.ensure_header();
        });
        recorder
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        #[cfg(test)]
        if let Some(waiting) = &self.lock_waiting {
            match self.state.try_lock() {
                Ok(state) => return state,
                Err(std::sync::TryLockError::Poisoned(error)) => return error.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    waiting.wait();
                }
            }
        }
        match self.state.lock() {
            Ok(state) => state,
            Err(error) => error.into_inner(),
        }
    }

    fn with_state<T>(&self, work: impl FnOnce(&mut State) -> T) -> Option<T> {
        let mut state = self.lock();
        if DISABLED.load(Ordering::Acquire) {
            return None;
        }
        let mut unwind = DisableOnUnwind { completed: false };
        let result = work(&mut state);
        unwind.completed = true;
        drop(unwind);
        let messages = std::mem::take(&mut state.pending_diagnostics);
        drop(state);
        for message in messages {
            self.report(&message);
        }
        Some(result)
    }

    fn report(&self, message: &str) {
        #[cfg(test)]
        if let Some(hook) = &self.diagnostic_hook {
            let free = matches!(
                self.state.try_lock(),
                Ok(_) | Err(std::sync::TryLockError::Poisoned(_))
            );
            hook(message, free);
        }
        diagnostic(message);
    }

    fn now(&self) -> u64 {
        #[cfg(test)]
        if let Some(clock) = &self.clock {
            return clock.load(Ordering::Relaxed);
        }
        self.epoch.elapsed().as_nanos().min(u64::MAX as u128) as u64
    }

    pub(super) fn device(&self) -> u64 {
        self.with_state(|state| {
            let dev = state.next_device;
            let Some(next) = dev.checked_add(1) else {
                return 0;
            };
            state.next_device = next;
            state.devices.insert(dev, Device::default());
            dev
        })
        .unwrap_or(0)
    }

    pub(super) fn begin(
        &self,
        dev: u64,
        cdb: &[u8],
        dir: Direction,
        requested_bytes: usize,
        timeout_ms: u32,
    ) -> Option<Command> {
        guarded(|| self.begin_inner(dev, cdb, dir, requested_bytes, timeout_ms)).flatten()
    }

    fn begin_inner(
        &self,
        dev: u64,
        cdb: &[u8],
        dir: Direction,
        requested_bytes: usize,
        timeout_ms: u32,
    ) -> Option<Command> {
        let start_mono_ns = self.now();
        // SG_IO rejects longer CDBs. Never allocate or hex-encode that input.
        let key = Key {
            cdb: if cdb.len() <= 16 {
                Some(cdb.to_vec())
            } else {
                None
            },
            requested_bytes,
            timeout_ms,
        };
        let matching_run = self.with_state(|state| {
            let device = state.devices.get(&dev)?;
            let matching_run = device.run.as_ref().is_some_and(|run| run.key == key);
            if !matching_run {
                state.close_run(dev, start_mono_ns);
                state.row(
                    json!({"event":"start", "mono_ns":start_mono_ns, "dev":dev,
                "cdb":key.cdb.as_deref().map(hex), "dir":dir.as_str(),
                "requested_bytes":requested_bytes, "timeout_ms":timeout_ms}),
                    false,
                );
            }
            Some(matching_run)
        })??;
        let dispatch_mono_ns = self.now();
        Some(Command {
            dev,
            key,
            opcode: cdb.first().copied(),
            dir,
            start_mono_ns,
            dispatch_mono_ns,
            matching_run,
        })
    }

    /// Observe raw SG_IO before the transport propagates or converts its result.
    pub(super) fn complete(
        &self,
        command: Option<Command>,
        result: Result<usize, &ScsiError>,
        response: &[u8],
    ) {
        let _ = guarded(|| self.complete_inner(command, result, response));
    }

    fn complete_inner(
        &self,
        command: Option<Command>,
        result: Result<usize, &ScsiError>,
        response: &[u8],
    ) {
        let Some(command) = command else {
            return;
        };
        let now = self.now();
        let duration_ns = now.saturating_sub(command.dispatch_mono_ns);
        let full = result.is_ok_and(|n| n == command.key.requested_bytes);
        let _ = self.with_state(|state| {
            let Some(device) = state.devices.get_mut(&command.dev) else {
                return;
            };
            if command.matching_run && full {
                let Some(run) = device.run.as_mut() else {
                    return;
                };
                run.count = run.count.saturating_add(1);
                run.bytes = run.bytes.saturating_add(command.key.requested_bytes as u64);
                run.total_ns = run.total_ns.saturating_add(duration_ns);
                run.max_ns = run.max_ns.max(duration_ns);
                run.last_mono_ns = command.start_mono_ns;
                if now.saturating_sub(run.last_row_ns) >= RUN_INTERVAL_NS {
                    state.run_row(command.dev, now);
                }
                return;
            }
            if command.matching_run {
                state.close_run(command.dev, now);
            }
            let (outcome, transferred_bytes, error) = completion_fields(result);
            let mut row = json!({"event":"done", "mono_ns":now, "dev":command.dev,
            "opcode":command.opcode, "outcome":outcome, "transferred_bytes":transferred_bytes,
            "duration_ns":duration_ns, "error":error});
            if command.matching_run {
                if let Some(object) = row.as_object_mut() {
                    object.insert("cdb".into(), json!(command.key.cdb.as_deref().map(hex)));
                    object.insert("start_mono_ns".into(), json!(command.start_mono_ns));
                }
            }
            state.row(row, result.is_err());
            let unidentified = state
                .devices
                .get(&command.dev)
                .is_some_and(|device| !device.identified);
            let cdb = command.key.cdb.as_deref();
            if matches!(command.dir, Direction::In)
                && command.opcode == Some(0x12)
                && cdb
                    .and_then(|bytes| bytes.get(1))
                    .is_some_and(|byte| byte & 1 == 0)
                && unidentified
            {
                if let Ok(n) = result {
                    if let Some(prefix) = response.get(..n) {
                        if let Ok(inquiry) = Inquiry::parse(prefix) {
                            state.row(
                                json!({"event":"device", "mono_ns":now, "dev":command.dev,
                            "type":prefix.first().map(|byte| byte & 0x1f),
                            "vendor":String::from_utf8_lossy(&inquiry.vendor),
                            "product":String::from_utf8_lossy(&inquiry.product),
                            "revision":String::from_utf8_lossy(&inquiry.revision)}),
                                false,
                            );
                            if let Some(device) = state.devices.get_mut(&command.dev) {
                                device.identified = true;
                            }
                        }
                    }
                }
            }
            if full && cdb.is_some() && matches!(command.opcode, Some(0x08 | 0x0a)) {
                if let Some(device) = state.devices.get_mut(&command.dev) {
                    device.run = Some(Run {
                        bytes: command.key.requested_bytes as u64,
                        key: command.key,
                        count: 1,
                        total_ns: duration_ns,
                        max_ns: duration_ns,
                        first_mono_ns: command.start_mono_ns,
                        last_mono_ns: command.start_mono_ns,
                        last_row_ns: now,
                    });
                }
            }
        });
    }

    pub(super) fn close_device(&self, dev: u64) {
        let _ = guarded(|| {
            let _ = self.with_state(|state| {
                state.close_run(dev, self.now());
                state.devices.remove(&dev);
            });
        });
    }
}

impl State {
    fn ensure_header(&mut self) -> bool {
        if let Some(header) = self.pending_header.take() {
            if !self.write_row(&header) {
                self.pending_header = Some(header);
                return false;
            }
        }
        true
    }

    fn row(&mut self, row: Value, failure: bool) {
        if self.ensure_header() {
            self.write_row(&row);
        }
        if failure && !DISABLED.load(Ordering::Acquire) {
            self.sync_failure();
        }
    }

    fn write_row(&mut self, row: &Value) -> bool {
        let mut bytes = match serde_json::to_vec(row) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.warn_once(&error);
                return false;
            }
        };
        if self.needs_newline {
            bytes.insert(0, b'\n');
        }
        bytes.push(b'\n');
        let next = self.reserved_bytes.checked_add(bytes.len() as u64);
        if self
            .file_limit
            .is_some_and(|limit| next.is_none_or(|length| length > limit.saturating_sub(65_536)))
        {
            if !DISABLED.swap(true, Ordering::AcqRel) {
                self.pending_diagnostics
                    .push("rem: command trace disabled: file-size limit reached".into());
            }
            return false;
        }
        self.reserved_bytes = next.unwrap_or(u64::MAX);
        match self.writer.write_all(&bytes) {
            Ok(()) => {
                self.needs_newline = false;
                true
            }
            Err(error) => {
                // The error gives no count: even a zero-byte failure may need a separator.
                self.needs_newline = true;
                self.warn_once(&error);
                false
            }
        }
    }

    fn sync_failure(&mut self) {
        #[cfg(test)]
        {
            self.sync_count += 1;
        }
        if let Err(error) = self.writer.sync_data() {
            self.warn_once(&error);
        }
    }

    fn warn_once(&mut self, error: &dyn std::fmt::Display) {
        if !self.warned {
            self.warned = true;
            #[cfg(test)]
            {
                self.warning_count += 1;
            }
            self.pending_diagnostics
                .push(format!("rem: command trace write/sync failed: {error}"));
        }
    }

    fn run_row(&mut self, dev: u64, now: u64) {
        let Some(run) = self
            .devices
            .get_mut(&dev)
            .and_then(|device| device.run.as_mut())
        else {
            return;
        };
        run.last_row_ns = now;
        let row = json!({"event":"run", "mono_ns":now, "dev":dev,
            "cdb":run.key.cdb.as_deref().map(hex), "count":run.count, "bytes":run.bytes,
            "total_ns":run.total_ns, "max_ns":run.max_ns,
            "first_mono_ns":run.first_mono_ns, "last_mono_ns":run.last_mono_ns});
        self.row(row, false);
    }

    fn close_run(&mut self, dev: u64, now: u64) {
        if self
            .devices
            .get(&dev)
            .is_some_and(|device| device.run.is_some())
        {
            self.run_row(dev, now);
            if let Some(device) = self.devices.get_mut(&dev) {
                device.run = None;
            }
        }
    }
}

fn completion_fields(result: Result<usize, &ScsiError>) -> (&'static str, Option<usize>, Value) {
    match result {
        Ok(n) => ("good", Some(n), Value::Null),
        Err(error) => {
            let (name, fields) = match error {
                ScsiError::Cancelled => ("Cancelled", json!({})),
                ScsiError::Truncated { got, need } => {
                    ("Truncated", json!({"got": got, "need": need}))
                }
                ScsiError::InvalidResponse { offset, detail } => (
                    "InvalidResponse",
                    json!({"offset": offset, "detail": detail}),
                ),
                ScsiError::Io(error) => (
                    "Io",
                    json!({"errno": error.raw_os_error(), "message": error.to_string()}),
                ),
                ScsiError::CheckCondition {
                    sense,
                    bytes_transferred,
                } => (
                    "CheckCondition",
                    json!({"sense": hex(sense), "bytes_transferred": bytes_transferred}),
                ),
                ScsiError::UnexpectedStatus { status } => {
                    ("UnexpectedStatus", json!({"status": status}))
                }
                ScsiError::TransportError {
                    status,
                    host_status,
                    driver_status,
                    info,
                    sense,
                } => (
                    "TransportError",
                    json!({"status": status, "host_status": host_status, "driver_status": driver_status, "info": info, "sense": hex(sense)}),
                ),
                ScsiError::HostCapacityRefused(detail) => {
                    ("HostCapacityRefused", json!({"detail": detail}))
                }
                ScsiError::InvalidInput(detail) => ("InvalidInput", json!({"detail": detail})),
            };
            let transferred = match error {
                ScsiError::CheckCondition {
                    bytes_transferred, ..
                } => Some(*bytes_transferred as usize),
                _ => None,
            };
            (name, transferred, fields)
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(DIGITS[(byte >> 4) as usize] as char);
        text.push(DIGITS[(byte & 15) as usize] as char);
    }
    text
}

fn wall_utc(wall: SystemTime) -> Option<String> {
    let elapsed = wall.duration_since(UNIX_EPOCH).ok()?;
    let seconds = elapsed.as_secs();
    // Gregorian civil date from days since 1970-01-01, in 400-year eras.
    let days = i128::from(seconds / 86_400) + 719_468;
    let era = days / 146_097;
    let day_of_era = days % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = month_index + if month_index < 10 { 3 } else { -9 };
    let year = era * 400 + year_of_era + i128::from(month <= 2);
    if year > 9999 {
        return None;
    }
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60
    ))
}

#[cfg(test)]
mod tests;
