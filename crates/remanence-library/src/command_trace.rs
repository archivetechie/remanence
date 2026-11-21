//! Opt-in, process-wide command evidence at the Linux SG_IO funnel.
//!
//! Rows contain command metadata and raw errors, never transfer payloads. Only
//! standard INQUIRY's maker/model/revision fields may leave a response buffer.
//! Sequential READ(6)/WRITE(6) successes share cumulative runs per device. The
//! mutex protects rows and run accounting, and is released before every ioctl.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use remanence_scsi::{inquiry::Inquiry, ScsiError};
use serde_json::{json, Value};

const RUN_INTERVAL_NS: u64 = 10_000_000_000;
static PROCESS_TRACE: OnceLock<Option<Arc<Recorder>>> = OnceLock::new();

#[derive(Debug, PartialEq, Eq)]
enum Policy<'a> {
    Off,
    FieldTrace,
    Directory(&'a Path),
}

fn policy<'a>(trace: Option<&'a OsStr>, field_trace: Option<&OsStr>) -> Policy<'a> {
    if field_trace.is_some() {
        Policy::FieldTrace
    } else {
        match trace.filter(|value| !value.is_empty()) {
            Some(value) => Policy::Directory(Path::new(value)),
            None => Policy::Off,
        }
    }
}

/// Resolve the environment once, when the first transport is opened.
pub(super) fn process_recorder() -> Option<Arc<Recorder>> {
    PROCESS_TRACE
        .get_or_init(|| {
            let trace = std::env::var_os("REM_TRACE");
            let field_trace = std::env::var_os("REM_FIELD_TRACE");
            let mut report = |message: &str| eprintln!("{message}");
            match policy(trace.as_deref(), field_trace.as_deref()) {
                Policy::Off => None,
                Policy::FieldTrace => {
                    report("rem: command trace disabled while the field trace is on");
                    None
                }
                Policy::Directory(directory) => open_recorder(
                    directory,
                    std::process::id(),
                    SystemTime::now(),
                    &mut report,
                ),
            }
        })
        .clone()
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
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        Ok(file)
    })();
    match opened {
        Ok(file) => Some(Arc::new(Recorder::new(file, pid, wall))),
        Err(error) => {
            report(&format!("rem: command trace disabled: {error}"));
            None
        }
    }
}

pub(super) struct Recorder {
    state: Mutex<State>,
    epoch: Instant,
    #[cfg(test)]
    clock: Option<Arc<std::sync::atomic::AtomicU64>>,
}

struct State {
    file: File,
    devices: HashMap<u64, Device>,
    next_device: u64,
    warned: bool,
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
    cdb: Vec<u8>,
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
    dir: Direction,
    start_mono_ns: u64,
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
    #[cfg(test)]
    pub(super) fn for_test(directory: &Path) -> Arc<Self> {
        open_recorder(directory, 123, UNIX_EPOCH, &mut |reason| panic!("{reason}"))
            .expect("test trace opens")
    }

    #[cfg(test)]
    pub(super) fn sync_count(&self) -> usize {
        self.lock().sync_count
    }

    fn new(file: File, pid: u32, wall: SystemTime) -> Self {
        let recorder = Self {
            state: Mutex::new(State {
                file,
                devices: HashMap::new(),
                next_device: 1,
                warned: false,
                #[cfg(test)]
                sync_count: 0,
                #[cfg(test)]
                warning_count: 0,
            }),
            epoch: Instant::now(),
            #[cfg(test)]
            clock: None,
        };
        recorder.lock().row(
            json!({"event": "header", "mono_ns": recorder.now(), "schema": 1,
                "rem_version": env!("CARGO_PKG_VERSION"), "pid": pid,
                "wall": wall_utc(wall)}),
            false,
        );
        recorder
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn now(&self) -> u64 {
        #[cfg(test)]
        if let Some(clock) = &self.clock {
            return clock.load(std::sync::atomic::Ordering::Relaxed);
        }
        self.epoch.elapsed().as_nanos().min(u64::MAX as u128) as u64
    }

    pub(super) fn device(&self) -> u64 {
        let mut state = self.lock();
        let dev = state.next_device;
        state.next_device += 1;
        state.devices.insert(dev, Device::default());
        dev
    }

    pub(super) fn begin(
        &self,
        dev: u64,
        cdb: &[u8],
        dir: Direction,
        requested_bytes: usize,
        timeout_ms: u32,
    ) -> Command {
        let start_mono_ns = self.now();
        let key = Key {
            cdb: cdb.to_vec(),
            requested_bytes,
            timeout_ms,
        };
        let mut state = self.lock();
        let matching_run = state.devices[&dev]
            .run
            .as_ref()
            .is_some_and(|run| run.key == key);
        if !matching_run {
            state.close_run(dev, start_mono_ns);
            state.row(
                json!({"event": "start", "mono_ns": start_mono_ns, "dev": dev,
                    "cdb": hex(cdb), "dir": dir.as_str(), "requested_bytes": requested_bytes,
                    "timeout_ms": timeout_ms}),
                false,
            );
        }
        Command {
            dev,
            key,
            dir,
            start_mono_ns,
            matching_run,
        }
    }

    /// Observe the raw result before the transport applies `?` or conversions.
    pub(super) fn complete(
        &self,
        command: Command,
        result: Result<usize, &ScsiError>,
        response: &[u8],
    ) {
        let now = self.now();
        let duration_ns = now.saturating_sub(command.start_mono_ns);
        let full = result.is_ok_and(|n| n == command.key.requested_bytes);
        let mut state = self.lock();
        if command.matching_run && full {
            let run = state
                .devices
                .get_mut(&command.dev)
                .unwrap()
                .run
                .as_mut()
                .unwrap();
            run.count += 1;
            run.bytes += command.key.requested_bytes as u64;
            run.total_ns += duration_ns;
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
        let mut row = json!({"event": "done", "mono_ns": now, "dev": command.dev,
            "opcode": command.key.cdb.first(), "outcome": outcome,
            "transferred_bytes": transferred_bytes, "duration_ns": duration_ns,
            "error": error});
        if command.matching_run {
            row["cdb"] = json!(hex(&command.key.cdb));
            row["start_mono_ns"] = json!(command.start_mono_ns);
        }
        state.row(row, result.is_err());

        if matches!(command.dir, Direction::In)
            && command.key.cdb.first() == Some(&0x12)
            && command.key.cdb.get(1).is_some_and(|byte| byte & 1 == 0)
            && !state.devices[&command.dev].identified
        {
            if let Ok(n) = result {
                // Parse only bytes the raw completion says the device supplied.
                if let Some(prefix) = response.get(..n) {
                    if let Ok(inquiry) = Inquiry::parse(prefix) {
                        state.row(
                            json!({"event": "device", "mono_ns": now, "dev": command.dev,
                                "type": prefix[0] & 0x1f,
                                "vendor": String::from_utf8_lossy(&inquiry.vendor),
                                "product": String::from_utf8_lossy(&inquiry.product),
                                "revision": String::from_utf8_lossy(&inquiry.revision)}),
                            false,
                        );
                        state.devices.get_mut(&command.dev).unwrap().identified = true;
                    }
                }
            }
        }
        if full && matches!(command.key.cdb.first(), Some(0x08 | 0x0a)) {
            state.devices.get_mut(&command.dev).unwrap().run = Some(Run {
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

    pub(super) fn close_device(&self, dev: u64) {
        let mut state = self.lock();
        state.close_run(dev, self.now());
        state.devices.remove(&dev);
    }
}

impl State {
    fn row(&mut self, row: Value, failure: bool) {
        // Values here contain no fallible custom serializer. One write_all sends
        // the complete line directly to the file, with no userspace buffering.
        let mut bytes = serde_json::to_vec(&row).expect("trace row is JSON");
        bytes.push(b'\n');
        if let Err(error) = self.file.write_all(&bytes) {
            self.warn_once(&error);
        }
        if failure {
            self.sync_failure();
        }
    }

    fn sync_failure(&mut self) {
        #[cfg(test)]
        {
            self.sync_count += 1;
        }
        if let Err(error) = self.file.sync_data() {
            self.warn_once(&error);
        }
    }

    fn warn_once(&mut self, error: &std::io::Error) {
        if !self.warned {
            self.warned = true;
            #[cfg(test)]
            {
                self.warning_count += 1;
            }
            eprintln!("rem: command trace write/sync failed: {error}");
        }
    }

    fn run_row(&mut self, dev: u64, now: u64) {
        let run = self.devices.get_mut(&dev).unwrap().run.as_mut().unwrap();
        run.last_row_ns = now;
        let row = json!({"event": "run", "mono_ns": now, "dev": dev,
            "cdb": hex(&run.key.cdb), "count": run.count, "bytes": run.bytes,
            "total_ns": run.total_ns, "max_ns": run.max_ns,
            "first_mono_ns": run.first_mono_ns, "last_mono_ns": run.last_mono_ns});
        self.row(row, false);
    }

    fn close_run(&mut self, dev: u64, now: u64) {
        if self
            .devices
            .get(&dev)
            .is_some_and(|device| device.run.is_some())
        {
            self.run_row(dev, now);
            self.devices.get_mut(&dev).unwrap().run = None;
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
