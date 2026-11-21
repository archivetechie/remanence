//! [`SgTransport`] — the small abstraction Remanence layers call
//! through when they want to send a CDB to a SCSI-generic device.
//!
//! Three methods cover the three data directions rem uses:
//!
//! - `execute_in` (`SG_DXFER_FROM_DEV`) — discovery's INQUIRY / VPD /
//!   READ ELEMENT STATUS plus Layer 3a's READ / READ POSITION /
//!   MODE SENSE. Reads response bytes into a caller buffer.
//! - `execute_none` (`SG_DXFER_NONE`) — Layer 2b's state-changing
//!   primitives (MOVE MEDIUM, INITIALIZE ELEMENT STATUS, PREVENT /
//!   ALLOW MEDIUM REMOVAL, SSC LOAD/UNLOAD) plus Layer 3a's
//!   REWIND / LOCATE / SPACE / WRITE FILEMARKS. No data phase; the
//!   operation lives entirely in the CDB.
//! - `execute_out` (`SG_DXFER_TO_DEV`) — Layer 3a's WRITE and
//!   MODE SELECT.
//!
//! The data-direction split is structural, not merely conventional:
//! because discovery never calls `execute_none` or `execute_out`, a
//! discovery pass is mechanically incapable of emitting a state-
//! changing CDB.
//!
//! `execute_in` and `execute_out` return [`TransferOutcome`] carrying
//! `bytes_transferred` + optional decoded [`SenseInfo`] — Layer 3a
//! needs the residual-bytes + sense fields to construct accurate
//! `WriteOutcome` / `ReadBufferTooSmall` / EOM signalling. Pre-
//! Layer-3a callers (discovery, refresh) just use `.bytes_transferred
//! as usize` and ignore the sense field.
//!
//! Production code uses [`LinuxSgTransport`] which wraps the
//! corresponding functions in [`remanence_scsi::sg_io`]. Tests use
//! [`FixtureTransport`] to feed canned bytes / log CDBs through the
//! same code path the daemon takes.
//!
//! Anything fancier (concurrency, retry, latency tracking) is layered
//! above; this module only owes the three `execute_*` methods.

use std::sync::{Arc, Mutex, MutexGuard};

use remanence_scsi::ScsiError;

/// Operational class of a CDB. Used to pick an appropriate SG_IO
/// timeout for the next call — real tape libraries are slow, and
/// a single global timeout would either be too tight for MOVE /
/// LOAD / INIT or wastefully loose for INQUIRY.
///
/// Numbers come from a mix of SMC-3 / SSC guidance and operator
/// experience on the MSL3040: MOVE MEDIUM commonly takes 8–20 s
/// on a real chassis (with worst-case grippper retries longer);
/// INITIALIZE ELEMENT STATUS walks every slot and can take
/// minutes on a 40-slot library; SSC LOAD on an LTO drive can
/// thread the tape and seek to BOT, also minutes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutClass {
    /// INQUIRY / VPD handshake commands. Sub-100 ms on real
    /// hardware; we still allow 5 s for slow / loaded hosts.
    Inquiry,
    /// READ ELEMENT STATUS. Roughly linear in element count;
    /// 60 s is comfortable for libraries up to a few hundred
    /// elements.
    ReadElementStatus,
    /// MOVE MEDIUM. The robot picks the cartridge, traverses the
    /// chassis, and places it. 8–20 s typical, longer on retries
    /// or with gripper resets — give it 120 s before declaring
    /// the CDB hung.
    Move,
    /// INITIALIZE ELEMENT STATUS. The robot re-derives its
    /// element map from physical inventory; for large libraries
    /// this can run to multiple minutes. 600 s (10 min) is the
    /// conservative upper bound.
    InitElementStatus,
    /// SSC LOAD / UNLOAD on a tape drive. Includes mechanical
    /// load/unload plus tape positioning; LTO-9 LOAD can take
    /// several minutes from cold. 600 s.
    LoadUnload,
    /// PREVENT / ALLOW MEDIUM REMOVAL. A config-only CDB the
    /// firmware applies immediately. 5 s is generous.
    PreventAllow,
    /// Block-level READ / WRITE on a loaded tape (Layer 3a). 900 s
    /// per CDB, matching the normal tape-driver timeout so drive-side
    /// recovery and buffered destage are not aborted by the host.
    TapeIo,
    /// WRITE FILEMARKS (forces sync to media unless IMMED is set;
    /// rem doesn't set IMMED). 900 s for drive recovery and destage.
    WriteFilemarks,
    /// LOCATE — moves the heads without reading or writing user
    /// data. 300 s covers the LTO-9
    /// worst case (~100 s BOT → EOT) plus retries.
    Positioning,
    /// SPACE — relative motion or a search for end of data. 3,600 s.
    /// An LTO-6 took about 16 minutes to answer a SPACE that met a lost end-of-data mark.
    Space,
    /// REWIND — a full-tape return to BOT from arbitrary position.
    /// 600 s.
    Rewind,
    /// MODE SELECT / MODE SENSE — config CDBs the drive applies
    /// immediately. 5 s.
    ModeConfig,
    /// READ POSITION — short config read, sub-100 ms typical. 5 s.
    TapeStatus,
}

impl TimeoutClass {
    /// Translate the class into the milliseconds value
    /// [`LinuxSgTransport`] passes to `SG_IO`.
    pub fn duration_ms(self) -> u32 {
        match self {
            Self::Inquiry => 5_000,
            Self::PreventAllow => 5_000,
            Self::ModeConfig => 5_000,
            Self::TapeStatus => 5_000,
            Self::TapeIo => 900_000,
            Self::ReadElementStatus => 60_000,
            Self::Move => 120_000,
            Self::WriteFilemarks => 900_000,
            Self::Positioning => 300_000,
            Self::Space => 3_600_000,
            Self::InitElementStatus => 600_000,
            Self::LoadUnload => 600_000,
            Self::Rewind => 600_000,
        }
    }
}

/// Outcome of a successful or short SCSI data-transfer command
/// returned by [`SgTransport::execute_in`] / `execute_out`.
///
/// `bytes_transferred` is the count `dxfer_len - resid` from the
/// SG_IO header — what the drive actually moved to/from the buffer.
/// May be less than `buf.len()` on:
/// - Short reads in variable-block mode (the on-tape block was
///   smaller than the host buffer; ILI flag in sense data).
/// - Truncated reads when the host buffer was too small for the
///   on-tape block (ILI; the block has been consumed).
/// - Early-warning / EOM during writes (the drive committed up
///   to the warning point and stopped).
///
/// `sense` is `Some(...)` when the drive returned sense bytes
/// without going CHECK CONDITION — the deferred / informational
/// sense path. Used by SSC drives for ILI, EOM, FILEMARK, and
/// near-EOM warnings. CHECK CONDITION still goes through the
/// `Err(ScsiError::CheckCondition { sense })` path; this struct
/// covers the "command succeeded with informational sense"
/// case which has historically required callers to re-parse the
/// raw bytes.
#[derive(Debug, Clone)]
pub struct TransferOutcome {
    /// Bytes actually transferred (read into the caller's `buf` for
    /// `execute_in`; written from the caller's `buf` for `execute_out`).
    pub bytes_transferred: u32,
    /// Decoded sense data if the drive returned any without going
    /// CHECK CONDITION. `None` on the no-sense-set happy path.
    pub sense: Option<SenseInfo>,
}

impl TransferOutcome {
    /// Construct a clean transfer outcome — happy-path success with
    /// no informational sense.
    pub fn clean(bytes_transferred: u32) -> Self {
        Self {
            bytes_transferred,
            sense: None,
        }
    }
}

/// Decoded sense data from an SSC data-transfer CDB. The fields
/// rem cares about; the raw sense bytes are still available via
/// the [`ScsiError`] error path when the drive went CHECK CONDITION.
#[derive(Debug, Clone)]
pub struct SenseInfo {
    /// Sense key (4 bits, low nibble of sense byte 2). 0=No Sense,
    /// 1=Recovered Error, 2=Not Ready, 3=Medium Error, etc.
    pub key: u8,
    /// Additional Sense Code (sense byte 12).
    pub asc: u8,
    /// Additional Sense Code Qualifier (sense byte 13).
    pub ascq: u8,
    /// INFORMATION field (sense bytes 3..7 for fixed-format).
    /// `Some(...)` iff the VALID bit is set (sense byte 0 bit 7).
    /// For READ with ILI this is the actual on-tape block size in
    /// fixed-block mode, or the residual byte count in variable-
    /// block mode (twos-complement, can be negative — caller
    /// interprets in context).
    pub information: Option<u64>,
    /// ILI bit (sense byte 2 bit 5). Set when the drive's
    /// transfer length didn't match the host buffer / configured
    /// fixed-block size. For READ in variable-block this means
    /// the on-tape block had a different size than the buffer
    /// (block consumed; `space(-1, Blocks)` to retry).
    pub ili: bool,
    /// EOM bit (sense byte 2 bit 6). Set when the drive reached
    /// logical end-of-medium or its near-EOM (early-warning)
    /// threshold.
    pub eom: bool,
    /// FILEMARK bit (sense byte 2 bit 7). Set when the operation
    /// stopped at a file mark (READ that hit a mark; SPACE that
    /// completed early on a mark).
    pub filemark: bool,
}

/// Three data-direction methods cover the CDBs rem actually sends:
/// `execute_in` (data-in), `execute_none` (no data phase), and
/// `execute_out` (data-out — Layer 3a's WRITE / MODE SELECT).
///
/// `execute_in` and `execute_out` both return [`TransferOutcome`],
/// which carries `bytes_transferred` + optional decoded sense info
/// — Layer 3a needs both to construct accurate `WriteOutcome` /
/// `ReadBufferTooSmall` / EOM signalling. Pre-Layer-3a callers
/// (discovery, refresh) only use `.bytes_transferred` and ignore
/// the sense field.
pub trait SgTransport: Send {
    /// Issue the CDB with `SG_DXFER_FROM_DEV` and fill `buf` with the
    /// response. Used by **discovery** (INQUIRY / VPD / READ ELEMENT
    /// STATUS) and by **Layer 3a** for READ(6) / READ POSITION /
    /// MODE SENSE.
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError>;

    /// Issue the CDB with `SG_DXFER_NONE` — no data phase in either
    /// direction. Used by **Layer 2b** state-changing operations
    /// (MOVE MEDIUM, INITIALIZE ELEMENT STATUS, PREVENT/ALLOW
    /// MEDIUM REMOVAL, SSC LOAD/UNLOAD) and by **Layer 3a** for
    /// REWIND / LOCATE / SPACE / WRITE FILEMARKS.
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError>;

    /// Issue the CDB with `SG_DXFER_TO_DEV` — write `buf` to the
    /// device as the command's data-out phase. Used by **Layer 3a**
    /// for WRITE(6) and MODE SELECT.
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError>;

    /// Set the per-CDB timeout for the *next* `execute_*` call.
    /// Production transports map `class` to a millisecond value via
    /// [`TimeoutClass::duration_ms`]; test transports default to
    /// no-op since they never block on a real device.
    ///
    /// Callers should issue this immediately before the matching
    /// `execute_*` call. The setting persists until the next
    /// `set_timeout_for` — there's no reset; the next op is
    /// expected to set its own class.
    fn set_timeout_for(&mut self, _class: TimeoutClass) {}

    /// Request the sg reserved buffer size used for large SG_IO data
    /// transfers and return the actual size the transport can provide.
    /// Non-Linux/test transports report the request as achieved.
    fn configure_reserved_buffer(&mut self, requested_bytes: u32) -> Result<u32, ScsiError> {
        Ok(requested_bytes)
    }
}

/// Blanket impl so callers holding a `Box<dyn SgTransport>` can pass
/// it directly to anything generic over `T: SgTransport` (notably
/// [`RecordingTransport`]).
impl SgTransport for Box<dyn SgTransport> {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        (**self).execute_in(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        (**self).execute_none(cdb)
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        (**self).execute_out(cdb, buf)
    }
    fn set_timeout_for(&mut self, class: TimeoutClass) {
        (**self).set_timeout_for(class)
    }
    fn configure_reserved_buffer(&mut self, requested_bytes: u32) -> Result<u32, ScsiError> {
        (**self).configure_reserved_buffer(requested_bytes)
    }
}

// ====================================================================
//  ForeignDriveTransport — CDB allowlist for non-rem drives
// ====================================================================

/// Read-only CDB gate for drives owned by another library/application.
///
/// The wrapper enforces the DS-M1 foreign-drive contract at the transport
/// boundary. Default mode permits identity/status reads and cumulative error
/// counter LOG SENSE pages only; TapeAlert page 0x2e is admitted only with the
/// explicit opt-in constructor because many drives clear those bits on read.
pub struct ForeignDriveTransport<T> {
    inner: T,
    foreign_tapealert: bool,
}

impl<T> ForeignDriveTransport<T> {
    /// Wrap `inner` with the default foreign-drive allowlist.
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            foreign_tapealert: false,
        }
    }

    /// Wrap `inner`, optionally allowing TapeAlert LOG SENSE page 0x2e.
    pub fn with_tapealert(inner: T, foreign_tapealert: bool) -> Self {
        Self {
            inner,
            foreign_tapealert,
        }
    }

    /// Borrow the wrapped transport.
    pub fn inner(&self) -> &T {
        &self.inner
    }

    /// Mutably borrow the wrapped transport.
    pub fn inner_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    /// Return the wrapped transport.
    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: SgTransport> SgTransport for ForeignDriveTransport<T> {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        if !foreign_drive_allows_execute_in(cdb, self.foreign_tapealert) {
            return Err(ScsiError::InvalidInput(
                "foreign drive transport blocked non-allowlisted data-in CDB",
            ));
        }
        self.inner.execute_in(cdb, buf)
    }

    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        if !matches!(cdb.first(), Some(0x00)) {
            return Err(ScsiError::InvalidInput(
                "foreign drive transport blocked non-allowlisted no-data CDB",
            ));
        }
        self.inner.execute_none(cdb)
    }

    fn execute_out(&mut self, _cdb: &[u8], _buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        Err(ScsiError::InvalidInput(
            "foreign drive transport blocked data-out CDB",
        ))
    }

    fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.inner.set_timeout_for(class)
    }

    fn configure_reserved_buffer(&mut self, requested_bytes: u32) -> Result<u32, ScsiError> {
        self.inner.configure_reserved_buffer(requested_bytes)
    }
}

fn foreign_drive_allows_execute_in(cdb: &[u8], foreign_tapealert: bool) -> bool {
    match cdb.first() {
        Some(0x12) => true, // INQUIRY, including VPD pages.
        Some(0xb8) => true, // READ ELEMENT STATUS.
        Some(0x4d) => {
            let Some(byte_2) = cdb.get(2) else {
                return false;
            };
            let page_code = byte_2 & 0x3f;
            matches!(page_code, 0x02 | 0x03) || (foreign_tapealert && page_code == 0x2e)
        }
        _ => false,
    }
}

// ====================================================================
//  LinuxSgTransport — the production transport
// ====================================================================

/// Production transport: wraps an open `File` for `/dev/sgN` and
/// dispatches each CDB through the kernel's `SG_IO` ioctl.
#[cfg(target_os = "linux")]
pub struct LinuxSgTransport {
    file: std::fs::File,
    /// Per-CDB timeout in milliseconds (0 = kernel default). The
    /// initial value is [`TimeoutClass::Inquiry`]'s window, which
    /// covers `INQUIRY / VPD / RES` for small libraries; callers
    /// should call [`SgTransport::set_timeout_for`] before each
    /// slow op (MOVE / INIT / LOAD-UNLOAD).
    timeout_ms: u32,
    trace: Option<Arc<crate::command_trace::Recorder>>,
    device_index: u64,
}

#[cfg(target_os = "linux")]
impl LinuxSgTransport {
    /// Open `/dev/sgN` read-only — the **discovery** path. Linux SG_IO
    /// has accepted `O_RDONLY` for `FROM_DEV` transfers since 2.6.18,
    /// so this works for INQUIRY / VPD / READ ELEMENT STATUS. The
    /// `Library::open(policy)` path uses [`Self::open_rw`] instead;
    /// reserve this constructor for read-only flows.
    ///
    /// On EACCES from `O_RDONLY` (some kernels reject it for SG nodes
    /// owned by `disk`/`tape` groups) the call falls back to
    /// read/write — preserves the pre-§7.6 behaviour for hosts whose
    /// permissions only allow the combined mode.
    pub fn open(path: &std::path::Path) -> Result<Self, std::io::Error> {
        if path.starts_with("/dev") {
            crate::sysfs::assert_live_scsi_allowed();
        }
        let trace = crate::command_trace::process_recorder();
        use std::fs::OpenOptions;
        let file = match OpenOptions::new().read(true).open(path) {
            Ok(file) => file,
            Err(err) if should_retry_read_only_open_as_read_write(&err) => {
                OpenOptions::new().read(true).write(true).open(path)?
            }
            Err(err) => return Err(err),
        };
        Ok(Self {
            file,
            timeout_ms: 5_000,
            device_index: trace.as_ref().map_or(0, |recorder| recorder.device()),
            trace,
        })
    }

    /// Open `/dev/sgN` read/write — the **state-changing handle** path.
    /// Use this for [`crate::Library::open`].
    ///
    /// Note: the four Layer 2b primitives (MOVE MEDIUM, INITIALIZE
    /// ELEMENT STATUS, PREVENT / ALLOW MEDIUM REMOVAL, SSC LOAD /
    /// UNLOAD) are all `SG_DXFER_NONE` — no data phase in either
    /// direction. Opening R/W is still the right thing because the
    /// Linux SG layer requires write access on the fd before it will
    /// authorise several of these opcodes, and because any future
    /// `SG_DXFER_TO_DEV` CDBs we add (MODE SELECT, etc.) won't
    /// surprise-fail on a read-only fd. Capability checks
    /// (`CAP_SYS_RAWIO`) are a separate gate.
    pub fn open_rw(path: &std::path::Path) -> Result<Self, std::io::Error> {
        if path.starts_with("/dev") {
            crate::sysfs::assert_live_scsi_allowed();
        }
        let trace = crate::command_trace::process_recorder();
        use std::fs::OpenOptions;
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(Self {
            file,
            timeout_ms: 5_000,
            device_index: trace.as_ref().map_or(0, |recorder| recorder.device()),
            trace,
        })
    }
}

#[cfg(target_os = "linux")]
fn should_retry_read_only_open_as_read_write(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::PermissionDenied
}

#[cfg(target_os = "linux")]
impl SgTransport for LinuxSgTransport {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        let command = self.trace.as_ref().map(|recorder| {
            recorder.begin(
                self.device_index,
                cdb,
                crate::command_trace::Direction::In,
                buf.len(),
                self.timeout_ms,
            )
        });
        let result = remanence_scsi::sg_io::execute_in(&self.file, cdb, buf, self.timeout_ms);
        if let (Some(recorder), Some(command)) = (&self.trace, command) {
            recorder.complete(command, result.as_ref().copied(), buf);
        }
        let n = result?;
        // The current sg_io::execute_in surfaces sense bytes only on
        // the error paths (CheckCondition / TransportError). On the
        // happy path the kernel returned status=GOOD with no sense
        // written, so there's no informational sense to decode.
        // Layer 3a's deferred-sense (ILI / EOM / FILEMARK on Ok())
        // case is reachable today only via the TransportError path,
        // and the sense-decode helper there parses the same bytes.
        Ok(TransferOutcome::clean(n as u32))
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        let command = self.trace.as_ref().map(|recorder| {
            recorder.begin(
                self.device_index,
                cdb,
                crate::command_trace::Direction::None,
                0,
                self.timeout_ms,
            )
        });
        let result = remanence_scsi::sg_io::execute_none(&self.file, cdb, self.timeout_ms);
        if let (Some(recorder), Some(command)) = (&self.trace, command) {
            recorder.complete(command, result.as_ref().map(|_| 0), &[]);
        }
        result
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        let command = self.trace.as_ref().map(|recorder| {
            recorder.begin(
                self.device_index,
                cdb,
                crate::command_trace::Direction::Out,
                buf.len(),
                self.timeout_ms,
            )
        });
        let result = remanence_scsi::sg_io::execute_out(&self.file, cdb, buf, self.timeout_ms);
        if let (Some(recorder), Some(command)) = (&self.trace, command) {
            recorder.complete(command, result.as_ref().copied(), &[]);
        }
        let n = result?;
        // execute_out's low-level wrapper returns bytes
        // transferred; Layer 3a's `WriteOutcome` needs the count
        // plus any informational sense. Today the sg_io wrapper
        // does not surface sense on Ok (CHECK CONDITION is the
        // only sense-carrying path), so `TransferOutcome::clean`
        // is correct for the success branch.
        Ok(TransferOutcome::clean(n as u32))
    }
    fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.timeout_ms = class.duration_ms();
    }

    fn configure_reserved_buffer(&mut self, requested_bytes: u32) -> Result<u32, ScsiError> {
        remanence_scsi::sg_io::set_reserved_size(&self.file, requested_bytes)
    }
}

#[cfg(target_os = "linux")]
impl Drop for LinuxSgTransport {
    fn drop(&mut self) {
        if let Some(recorder) = &self.trace {
            recorder.close_device(self.device_index);
        }
    }
}

// ====================================================================
//  FixtureTransport — for tests
// ====================================================================

/// In-memory transport that replays a fixed sequence of responses.
/// Used by `discover()` tests to drive the orchestration logic against
/// captured bytes without touching `/dev/sg*`.
///
/// The transport hands out responses in the order they were pushed.
/// Tests construct one per simulated device and seed it with exactly
/// the responses the device is expected to return for the CDBs
/// discovery will issue, in order. If discovery asks for one CDB more
/// than the script provides, the transport returns
/// `ScsiError::InvalidInput`.
pub struct FixtureTransport {
    responses: std::collections::VecDeque<Vec<u8>>,
    /// Tape of CDB bytes the transport was asked to execute, in order.
    /// Tests use this to assert that discovery issued only read-only
    /// CDBs and never sent a state-changing opcode.
    pub cdb_log: Vec<Vec<u8>>,
}

impl FixtureTransport {
    /// Empty transport — `push_response` to seed it.
    pub fn new() -> Self {
        Self {
            responses: std::collections::VecDeque::new(),
            cdb_log: Vec::new(),
        }
    }

    /// Append a response to the queue. Each `execute_in` call pops one.
    pub fn push_response(&mut self, response: impl Into<Vec<u8>>) -> &mut Self {
        self.responses.push_back(response.into());
        self
    }

    /// Convenience: append a sequence of responses in one call.
    pub fn with_responses<I, R>(mut self, responses: I) -> Self
    where
        I: IntoIterator<Item = R>,
        R: Into<Vec<u8>>,
    {
        for r in responses {
            self.push_response(r);
        }
        self
    }
}

impl Default for FixtureTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl SgTransport for FixtureTransport {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        self.cdb_log.push(cdb.to_vec());
        let next = self.responses.pop_front().ok_or(ScsiError::InvalidInput(
            "FixtureTransport: out of canned responses (test fixture under-seeded)",
        ))?;
        let n = next.len().min(buf.len());
        buf[..n].copy_from_slice(&next[..n]);
        Ok(TransferOutcome::clean(n as u32))
    }

    /// State-changing CDBs need no canned response — the operation
    /// either succeeds (`Ok(())`) or the test author wraps this
    /// transport with something that simulates failure. Default
    /// behaviour is "succeed" so the CDB log is what tests assert on.
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        self.cdb_log.push(cdb.to_vec());
        Ok(())
    }

    /// Data-out CDB in fixture mode: same shape as execute_none —
    /// log the CDB, return `Ok(TransferOutcome::clean(buf.len()))`.
    /// Tests assert on the CDB log + the buf the test author
    /// supplies; no canned response needed because the drive
    /// doesn't return data on a data-out CDB.
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        self.cdb_log.push(cdb.to_vec());
        Ok(TransferOutcome::clean(buf.len() as u32))
    }
}

// =====================================================================
//  RecordingTransport — generic wrapper for "what CDBs went out?" tests
// =====================================================================

/// Shared CDB log used by [`RecordingTransport`].
#[derive(Clone, Default)]
pub struct RecordingLog {
    inner: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl RecordingLog {
    /// Create an empty recording log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Borrow the recorded CDBs.
    pub fn borrow(&self) -> MutexGuard<'_, Vec<Vec<u8>>> {
        self.inner.lock().expect("recording log lock")
    }

    /// Mutably borrow the recorded CDBs.
    pub fn borrow_mut(&self) -> MutexGuard<'_, Vec<Vec<u8>>> {
        self.inner.lock().expect("recording log lock")
    }
}

/// Wraps any [`SgTransport`] and tees every CDB through a shared log.
/// The log handle is returned alongside the wrapped transport so tests
/// can introspect it after the operation under test has run.
///
/// Used by:
/// - Layer 2a's safety test "discovery issues only read-only CDBs."
/// - Layer 2b's tests "the right MOVE MEDIUM / LOAD / etc. CDB went
///   out, in the right order."
///
/// Both `execute_in` and `execute_none` are recorded; tests can sort
/// on opcode (CDB byte 0) to assert which CDBs hit which path.
pub struct RecordingTransport<T> {
    inner: T,
    log: RecordingLog,
}

impl<T> RecordingTransport<T> {
    /// Wrap `inner` with a fresh log. Returns
    /// `(wrapped_transport, log_handle)`. The log handle is a shared
    /// [`RecordingLog`] so the test can hold onto it while the wrapper
    /// is consumed by the code under test.
    pub fn new(inner: T) -> (Self, RecordingLog) {
        let log = RecordingLog::new();
        let s = Self {
            inner,
            log: log.clone(),
        };
        (s, log)
    }

    /// Wrap `inner` with a *shared* log. Use this when the test
    /// produces multiple `RecordingTransport`s (e.g., one per
    /// discovered `/dev/sgN`) and wants every CDB across every device
    /// merged into a single log for assertion.
    pub fn with_log(inner: T, log: RecordingLog) -> Self {
        Self { inner, log }
    }
}

impl<T: SgTransport> SgTransport for RecordingTransport<T> {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        self.log.borrow_mut().push(cdb.to_vec());
        self.inner.execute_in(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        self.log.borrow_mut().push(cdb.to_vec());
        self.inner.execute_none(cdb)
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        self.log.borrow_mut().push(cdb.to_vec());
        self.inner.execute_out(cdb, buf)
    }
    fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.inner.set_timeout_for(class)
    }

    fn configure_reserved_buffer(&mut self, requested_bytes: u32) -> Result<u32, ScsiError> {
        self.inner.configure_reserved_buffer(requested_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercise real ioctl failures on regular files, without a live SCSI node.
    #[cfg(target_os = "linux")]
    #[test]
    fn command_trace_all_bodies_preserve_raw_io_and_sync_each_failure_once() {
        let directory = tempfile::tempdir().unwrap();
        let device_file = tempfile::NamedTempFile::new().unwrap();
        let recorder = crate::command_trace::Recorder::for_test(directory.path());
        let mut transport = LinuxSgTransport::open_rw(device_file.path()).unwrap();
        transport.device_index = recorder.device();
        transport.trace = Some(Arc::clone(&recorder));
        SgTransport::set_timeout_for(&mut transport, TimeoutClass::TapeIo);
        for (index, dir) in ["in", "none", "out"].into_iter().enumerate() {
            let cdb = [0x12, 0, 0, 0, 36, 0];
            let mut buffer = [0u8; 36];
            let raw = match dir {
                "in" => remanence_scsi::sg_io::execute_in(
                    &transport.file,
                    &cdb,
                    &mut buffer,
                    transport.timeout_ms,
                )
                .map(|_| ()),
                "none" => {
                    remanence_scsi::sg_io::execute_none(&transport.file, &cdb, transport.timeout_ms)
                }
                "out" => remanence_scsi::sg_io::execute_out(
                    &transport.file,
                    &cdb,
                    &buffer,
                    transport.timeout_ms,
                )
                .map(|_| ()),
                _ => unreachable!(),
            };
            let result = match dir {
                "in" => SgTransport::execute_in(&mut transport, &cdb, &mut buffer).map(|_| ()),
                "none" => SgTransport::execute_none(&mut transport, &cdb),
                "out" => SgTransport::execute_out(&mut transport, &cdb, &buffer).map(|_| ()),
                _ => unreachable!(),
            };
            let (ScsiError::Io(expected), ScsiError::Io(actual)) =
                (raw.unwrap_err(), result.unwrap_err())
            else {
                panic!("regular file SG_IO must return Io");
            };
            assert_eq!(actual.raw_os_error(), Some(libc::ENOTTY));
            assert_eq!(actual.raw_os_error(), expected.raw_os_error());
            assert_eq!(actual.kind(), expected.kind());
            assert_eq!(actual.to_string(), expected.to_string());
            let text =
                std::fs::read_to_string(directory.path().join("rem-trace-123-0.jsonl")).unwrap();
            let rows: Vec<serde_json::Value> = text
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(rows.len(), 1 + (index + 1) * 2);
            let start = &rows[1 + index * 2];
            let done = &rows[2 + index * 2];
            assert_eq!(start["event"], "start");
            assert_eq!(done["event"], "done");
            assert_eq!(start["dev"], transport.device_index);
            assert_eq!(done["dev"], transport.device_index);
            assert_eq!(start["dir"], dir);
            assert_eq!(start["cdb"], "120000002400");
            assert_eq!(start["requested_bytes"], if dir == "none" { 0 } else { 36 });
            assert_eq!(start["timeout_ms"], 900_000);
            assert!(start["mono_ns"].as_u64().unwrap() <= done["mono_ns"].as_u64().unwrap());
            assert_eq!(done["outcome"], "Io");
            assert_eq!(done["opcode"], 0x12);
            assert!(done["transferred_bytes"].is_null());
            assert_eq!(done["error"]["errno"], libc::ENOTTY);
            assert_eq!(done["error"]["message"], expected.to_string());
            assert_eq!(
                recorder.sync_count(),
                index + 1,
                "one sync after each failure row"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn command_trace_transport_drop_flushes_pending_read_and_write_runs() {
        for (opcode, dir) in [
            (0x08, crate::command_trace::Direction::In),
            (0x0a, crate::command_trace::Direction::Out),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let device_file = tempfile::NamedTempFile::new().unwrap();
            let recorder = crate::command_trace::Recorder::for_test(directory.path());
            let mut transport = LinuxSgTransport::open_rw(device_file.path()).unwrap();
            transport.device_index = recorder.device();
            transport.trace = Some(Arc::clone(&recorder));
            for _ in 0..3 {
                let command = recorder.begin(
                    transport.device_index,
                    &[opcode, 0, 0, 0, 4, 0],
                    dir,
                    4,
                    5000,
                );
                recorder.complete(command, Ok(4), &[]);
            }
            let path = directory.path().join("rem-trace-123-0.jsonl");
            assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 3);
            drop(transport);
            let text = std::fs::read_to_string(&path).unwrap();
            let rows: Vec<serde_json::Value> = text
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(rows.len(), 4, "transport Drop must write the pending run");
            assert_eq!(rows[3]["event"], "run");
            assert_eq!(rows[3]["count"], 3);
            assert_eq!(rows[3]["bytes"], 12);
            assert_eq!(rows[3]["dev"], 1);
            assert_eq!(recorder.sync_count(), 0);
        }
    }

    /// Environment changes are confined to a subprocess running this test alone.
    #[cfg(target_os = "linux")]
    #[test]
    fn command_trace_process_child() {
        let Some(mode) = std::env::var_os("CT1_TRACE_CHILD") else {
            return;
        };
        let path = std::path::PathBuf::from(std::env::var_os("CT1_TRACE_DEVICE").unwrap());
        let first = LinuxSgTransport::open(&path).unwrap();
        let enabled = mode == "on";
        assert_eq!(first.trace.is_some(), enabled);
        if enabled {
            std::env::remove_var("REM_TRACE");
        } else {
            std::env::set_var("REM_TRACE", std::env::var_os("CT1_TRACE_CHANGED").unwrap());
        }
        std::env::remove_var("REM_FIELD_TRACE");
        let mut second = LinuxSgTransport::open_rw(&path).unwrap();
        assert_eq!(second.trace.is_some(), enabled, "switch is resolved once");
        if let (Some(first_recorder), Some(second_recorder)) = (&first.trace, &second.trace) {
            assert!(Arc::ptr_eq(first_recorder, second_recorder));
            assert_eq!(first.device_index, 1);
            assert_eq!(second.device_index, 2);
        }
        assert!(matches!(
            SgTransport::execute_none(&mut second, &[1, 0, 0, 0, 0, 0]),
            Err(ScsiError::Io(_))
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn command_trace_process_switch_is_once_and_disabled_diagnostics_are_once() {
        for mode in ["on", "off", "missing", "field"] {
            let directory = tempfile::tempdir().unwrap();
            let trace_directory = directory.path().join("traces");
            std::fs::create_dir(&trace_directory).unwrap();
            let device_file = tempfile::NamedTempFile::new().unwrap();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args([
                    "--exact",
                    "transport::tests::command_trace_process_child",
                    "--nocapture",
                ])
                .env("CT1_TRACE_CHILD", mode)
                .env("CT1_TRACE_DEVICE", device_file.path())
                .env("CT1_TRACE_CHANGED", &trace_directory)
                .env_remove("REM_FIELD_TRACE");
            match mode {
                "on" | "field" => {
                    child.env("REM_TRACE", &trace_directory);
                }
                "missing" => {
                    child.env("REM_TRACE", directory.path().join("missing"));
                }
                "off" => {
                    child.env("REM_TRACE", "");
                }
                _ => unreachable!(),
            }
            if mode == "field" {
                child.env("REM_FIELD_TRACE", "");
            }
            let output = child.output().unwrap();
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(
                output.status.success(),
                "{mode}: {stderr}\n{}",
                String::from_utf8_lossy(&output.stdout)
            );
            match mode {
                "on" | "off" => assert!(stderr.is_empty(), "{stderr}"),
                "field" => assert_eq!(
                    stderr,
                    "rem: command trace disabled while the field trace is on\n"
                ),
                "missing" => {
                    assert_eq!(stderr.lines().count(), 1);
                    assert!(stderr.starts_with("rem: command trace disabled: "));
                }
                _ => unreachable!(),
            }
            let files: Vec<_> = std::fs::read_dir(&trace_directory)
                .unwrap()
                .map(Result::unwrap)
                .collect();
            assert_eq!(files.len(), usize::from(mode == "on"));
            if mode == "on" {
                let text = std::fs::read_to_string(files[0].path()).unwrap();
                let rows: Vec<serde_json::Value> = text
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                assert_eq!(rows.len(), 3);
                assert_eq!(rows[0]["event"], "header");
                assert_eq!(rows[1]["dev"], 2);
                assert_eq!(rows[2]["dev"], 2);
            }
        }
    }

    #[test]
    fn fixture_transport_replays_in_order_and_logs_cdbs() {
        let mut t = FixtureTransport::new().with_responses([vec![0xaa, 0xbb], vec![0xcc]]);
        let mut buf = [0u8; 16];

        let n = t
            .execute_in(&[0x12, 0x00, 0x00, 0x00, 0x60, 0x00], &mut buf)
            .unwrap()
            .bytes_transferred as usize;
        assert_eq!(n, 2);
        assert_eq!(&buf[..n], &[0xaa, 0xbb]);

        let n = t
            .execute_in(&[0x12, 0x01, 0x80, 0x00, 0xfc, 0x00], &mut buf)
            .unwrap()
            .bytes_transferred as usize;
        assert_eq!(n, 1);
        assert_eq!(&buf[..n], &[0xcc]);

        // Third call has no canned response left.
        let r = t.execute_in(&[0x00], &mut buf);
        assert!(matches!(r, Err(ScsiError::InvalidInput(_))));

        // CDB log captures every call, verbatim.
        assert_eq!(t.cdb_log.len(), 3);
        assert_eq!(t.cdb_log[0][0], 0x12);
    }

    #[test]
    fn fixture_transport_execute_none_logs_and_returns_ok() {
        let mut t = FixtureTransport::new();
        // No canned response needed for execute_none.
        let cdb = [
            0xA5, 0x00, 0x00, 0x00, 0x03, 0xe9, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
        ];
        t.execute_none(&cdb).unwrap();
        assert_eq!(t.cdb_log.len(), 1);
        assert_eq!(t.cdb_log[0], &cdb[..]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_transport_retries_read_only_open_only_on_permission_denied() {
        let permission = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let missing = std::io::Error::from(std::io::ErrorKind::NotFound);

        assert!(should_retry_read_only_open_as_read_write(&permission));
        assert!(!should_retry_read_only_open_as_read_write(&missing));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_transport_set_timeout_for_updates_window() {
        // /dev/null is a valid fd; we never actually issue SG_IO,
        // we only assert that set_timeout_for mutates the per-CDB
        // ms field. The mapping is what handle.rs relies on.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap();
        let mut t = LinuxSgTransport {
            file,
            timeout_ms: TimeoutClass::Inquiry.duration_ms(),
            trace: None,
            device_index: 0,
        };
        assert_eq!(t.timeout_ms, 5_000);
        t.set_timeout_for(TimeoutClass::Move);
        assert_eq!(t.timeout_ms, 120_000);
        t.set_timeout_for(TimeoutClass::LoadUnload);
        assert_eq!(t.timeout_ms, 600_000);
        t.set_timeout_for(TimeoutClass::PreventAllow);
        assert_eq!(t.timeout_ms, 5_000);
        t.set_timeout_for(TimeoutClass::Space);
        assert_eq!(t.timeout_ms, 3_600_000);
    }

    #[test]
    fn recording_transport_tees_both_paths() {
        let inner = FixtureTransport::new().with_responses([vec![0xaa]]);
        let (mut rec, log) = RecordingTransport::new(inner);
        let mut buf = [0u8; 4];

        rec.execute_in(&[0x12, 0x00, 0x00, 0x00, 0x60, 0x00], &mut buf)
            .unwrap();
        rec.execute_none(&[0x07, 0x00, 0x00, 0x00, 0x00, 0x00])
            .unwrap();
        rec.execute_none(&[0x1B, 0x00, 0x00, 0x00, 0x00, 0x00])
            .unwrap();

        let log = log.borrow();
        assert_eq!(log.len(), 3);
        assert_eq!(log[0][0], 0x12);
        assert_eq!(log[1][0], 0x07);
        assert_eq!(log[2][0], 0x1B);
    }

    #[test]
    fn fixture_transport_execute_out_logs_and_reports_bytes() {
        // Variable-block WRITE(6) of a 4-byte payload through the
        // fixture transport. The transport records the CDB and
        // returns TransferOutcome::clean(buf.len()).
        let mut t = FixtureTransport::new();
        let cdb = [0x0A, 0x00, 0x00, 0x00, 0x04, 0x00]; // WRITE(6) len=4
        let payload = [0xDE, 0xAD, 0xBE, 0xEF];

        let outcome = t.execute_out(&cdb, &payload).expect("execute_out ok");
        assert_eq!(outcome.bytes_transferred, 4);
        assert!(outcome.sense.is_none(), "no sense on the happy path");
        assert_eq!(t.cdb_log.len(), 1);
        assert_eq!(t.cdb_log[0], &cdb[..]);
    }

    #[test]
    fn foreign_drive_transport_allows_read_only_identity_and_counter_cdbs() {
        let inner = FixtureTransport::new().with_responses([
            vec![0x00; 96],
            vec![0x00; 32],
            vec![0x00; 32],
        ]);
        let mut transport = ForeignDriveTransport::new(inner);
        let mut buf = [0u8; 96];

        transport
            .execute_in(&[0x12, 0x00, 0x00, 0x00, 0x60, 0x00], &mut buf)
            .expect("standard inquiry is allowed");
        transport
            .execute_in(&[0x4d, 0x00, 0x02, 0x00, 0x00, 0x00], &mut buf)
            .expect("write error counter page is allowed");
        transport
            .execute_in(&[0x4d, 0x00, 0x03, 0x00, 0x00, 0x00], &mut buf)
            .expect("read error counter page is allowed");
        transport
            .execute_none(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00])
            .expect("test unit ready is allowed");

        let inner = transport.into_inner();
        assert_eq!(inner.cdb_log.len(), 4);
        assert_eq!(inner.cdb_log[0][0], 0x12);
        assert_eq!(inner.cdb_log[3][0], 0x00);
    }

    #[test]
    fn foreign_drive_transport_rejects_mutating_or_clear_on_read_cdbs() {
        let inner = FixtureTransport::new().with_responses([vec![0x00; 96]]);
        let mut transport = ForeignDriveTransport::new(inner);
        let mut buf = [0u8; 96];

        assert!(matches!(
            transport.execute_in(&[0x4d, 0x00, 0x2e, 0x00, 0x00, 0x00], &mut buf),
            Err(ScsiError::InvalidInput(_))
        ));
        assert!(matches!(
            transport.execute_none(&[0x1b, 0x00, 0x00, 0x00, 0x01, 0x00]),
            Err(ScsiError::InvalidInput(_))
        ));
        assert!(matches!(
            transport.execute_out(&[0x0a, 0x00, 0x00, 0x00, 0x01, 0x00], &[0]),
            Err(ScsiError::InvalidInput(_))
        ));

        let mut transport = ForeignDriveTransport::with_tapealert(
            FixtureTransport::new().with_responses([vec![0x00; 96]]),
            true,
        );
        transport
            .execute_in(&[0x4d, 0x00, 0x2e, 0x00, 0x00, 0x00], &mut buf)
            .expect("TapeAlert page is allowed only with opt-in");
    }

    #[test]
    fn transfer_outcome_clean_helper_zero_sense() {
        let o = TransferOutcome::clean(1024);
        assert_eq!(o.bytes_transferred, 1024);
        assert!(o.sense.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn new_timeout_class_variants_have_documented_durations() {
        // Layer 3a's new timeout classes — pin the numeric mapping
        // so any accidental change to `duration_ms()` shows up here.
        assert_eq!(TimeoutClass::TapeIo.duration_ms(), 900_000);
        assert_eq!(TimeoutClass::WriteFilemarks.duration_ms(), 900_000);
        assert_eq!(TimeoutClass::Positioning.duration_ms(), 300_000);
        assert_eq!(TimeoutClass::Space.duration_ms(), 3_600_000);
        assert_eq!(TimeoutClass::Rewind.duration_ms(), 600_000);
        assert_eq!(TimeoutClass::ModeConfig.duration_ms(), 5_000);
        assert_eq!(TimeoutClass::TapeStatus.duration_ms(), 5_000);
    }
}
