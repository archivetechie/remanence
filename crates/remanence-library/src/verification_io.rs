//! Passive, drive-local diagnostics for one full verification. Scopes own shared
//! state rather than borrowing the drive, so nested raw adapters share a report.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::transport::TransferOutcome;
use remanence_scsi::{decode_sense, ScsiError};

/// Existing verification boundaries; nested scopes restore their enclosing phase.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VerificationPhase {
    /// Readiness, mode setup and bootstrap admission.
    #[default]
    Setup,
    /// Terminal discovery, prefix walk, replicas and separations.
    Walk,
    /// Directory preload and explicit ParityMap checks.
    ParityMap,
    /// Sidecar metadata, copies, footer and parity shards.
    Sidecar,
    /// Forward protected Object reads.
    ProtectedData,
    /// Unbuffered protected reads in the cached sidecar path.
    FallbackData,
    /// Structural recovery from BOT.
    BotRecovery,
}
impl VerificationPhase {
    /// Stable diagnostic names, in report order.
    pub const ALL: [Self; 7] = [
        Self::Setup,
        Self::Walk,
        Self::ParityMap,
        Self::Sidecar,
        Self::ProtectedData,
        Self::FallbackData,
        Self::BotRecovery,
    ];
    /// Wire/JSON name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::Walk => "walk_terminal",
            Self::ParityMap => "parity_map",
            Self::Sidecar => "sidecar",
            Self::ProtectedData => "protected_data",
            Self::FallbackData => "fallback_data",
            Self::BotRecovery => "bot_recovery",
        }
    }
}

/// Dispatch counts and timing, including unsuccessful attempts. Bytes include all data-in
/// and data-out transfers; records are exact READ transfers only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IoCounts {
    /// Wall time while this phase is current, excluding nested phases.
    pub elapsed_ms: u64,
    /// Wall time inside transport dispatches, including unsuccessful attempts.
    pub command_ms: u64,
    /// Longest transport dispatch.
    pub max_command_ms: u64,
    /// Command durations in ms: <5, [5,20), [20,100), [100,1000), [1000,10000), >=10000.
    pub command_latency: [u64; 6],
    /// LOCATE attempts.
    pub locate: u64,
    /// SPACE attempts.
    pub space: u64,
    /// READ POSITION attempts, including inline calls.
    pub read_position: u64,
    /// Variable single-record READ attempts.
    pub single_read: u64,
    /// Fixed READ attempts (including batches of one).
    pub batch_read: u64,
    /// All other dispatched CDBs.
    pub other: u64,
    /// READ records requested, including retries.
    pub records_requested: u64,
    /// Records from clean exact transfers or accepted recovered single reads.
    pub records_confirmed: u64,
    /// Transport-reported bytes, including unconfirmed transfers and setup.
    pub bytes: u64,
    /// Unclean hardware batches retried through the single-record reader.
    pub hardware_batch_fallbacks: u64,
    /// Accepted self-corrected single reads.
    pub recovered_reads: u64,
    /// Epochs needing at least one unbuffered data read.
    pub algorithmic_fallback_epochs: u64,
}

/// A contiguous physical read range. Repeated/overlapping ranges are preserved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IoRange {
    /// First physical LBA.
    pub start_lba: u64,
    /// Exclusive end LBA.
    pub end_lba: u64,
}

/// Diagnostic report. Never used to decide verification success.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VerificationIo {
    /// Counts over every phase.
    pub total: IoCounts,
    /// Counts indexed by VerificationPhase::ALL.
    pub phases: [IoCounts; 7],
    /// Monotonic elapsed time, including setup.
    pub elapsed_ms: u64,
    /// Protected LOCATE targets before the preceding protected read end.
    pub backward_data_seeks: u64,
    /// Confirmed protected sweep bytes (excludes fallback data).
    pub protected_bytes: u64,
    /// Confirmed sweep ranges in read order; duplicates remain visible.
    pub protected_ranges: Vec<IoRange>,
    /// Physical addresses of accepted self-corrected reads, in dispatch order.
    pub recovered_lbas: Vec<u64>,
    /// Accepted recovered reads whose physical address was not known.
    pub recovered_unknown_addresses: u64,
}
#[derive(Default)]
struct State {
    report: Option<VerificationIo>,
    control: Option<VerificationControl>,
    read_bytes: u64,
    tape_file: Option<u64>,
    phase: VerificationPhase,
    verification_depth: usize,
    position: Option<u64>,
    protected_end: Option<u64>,
    phase_started: Option<Instant>,
    phase_elapsed: [Duration; 7],
    phase_commands: [Duration; 7],
    command_elapsed: Duration,
}

/// Keep fractional milliseconds across phase entries and command dispatches.
fn milliseconds(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

impl State {
    /// Charge only the current phase, then start the next exclusive interval.
    fn account_phase(&mut self, now: Instant) {
        if let (Some(report), Some(started)) = (&mut self.report, self.phase_started) {
            let phase = self.phase as usize;
            self.phase_elapsed[phase] += now.duration_since(started);
            report.phases[phase].elapsed_ms = milliseconds(self.phase_elapsed[phase]);
            self.phase_started = Some(now);
        }
    }
}
/// Operation hooks scoped to a full verification. The observer never issues I/O.
struct VerificationControl {
    cancel: Arc<std::sync::atomic::AtomicBool>,
    observer: Arc<dyn Fn(VerificationPhase, u64, Option<u64>) + Send + Sync>,
}

/// Removes operation hooks when verification returns, including error paths.
pub struct VerificationControlScope(VerificationContext);
impl Drop for VerificationControlScope {
    fn drop(&mut self) {
        self.0 .0.lock().unwrap().control = None;
    }
}

/// Shared passive hooks for a drive and its source adapters.
#[derive(Clone, Default)]
pub struct VerificationContext(Arc<Mutex<State>>);
impl VerificationContext {
    /// Install cancellation and progress for this operation only.
    pub fn control(
        &self,
        cancel: Arc<std::sync::atomic::AtomicBool>,
        observer: Arc<dyn Fn(VerificationPhase, u64, Option<u64>) + Send + Sync>,
    ) -> VerificationControlScope {
        {
            let mut state = self.0.lock().unwrap();
            assert!(
                state.control.is_none(),
                "nested verification operation controls"
            );
            state.read_bytes = 0;
            state.tape_file = None;
            state.phase = VerificationPhase::Setup;
            state.control = Some(VerificationControl { cancel, observer });
        }
        self.notify();
        VerificationControlScope(self.clone())
    }

    /// Check immediately before each CDB, including commands inside raw adapters.
    /// An in-flight READ has the longest timeout: 900 seconds (TapeIo).
    pub fn check_cancelled(&self) -> Result<(), ScsiError> {
        self.notify();
        if self
            .0
            .lock()
            .unwrap()
            .control
            .as_ref()
            .is_some_and(|c| c.cancel.load(std::sync::atomic::Ordering::SeqCst))
        {
            Err(ScsiError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn notify(&self) {
        let state = self.0.lock().unwrap();
        let update = state.control.as_ref().map(|c| {
            (
                c.observer.clone(),
                state.phase,
                state.read_bytes,
                state.tape_file,
            )
        });
        drop(state);
        if let Some((observer, phase, bytes, tape_file)) = update {
            observer(phase, bytes, tape_file);
        }
    }

    /// Enter a phase without issuing commands.
    pub fn phase(&self, phase: VerificationPhase) -> VerificationScope {
        let entered = Instant::now();
        let mut state = self.0.lock().unwrap();
        state.account_phase(entered);
        let previous = state.phase;
        state.phase = phase;
        drop(state);
        self.notify();
        VerificationScope {
            context: self.clone(),
            previous: Some(previous),
        }
    }
    /// Enable verification's exact recovered-read semantics independently of counting.
    pub fn verification(&self) -> VerificationScope {
        self.0.lock().unwrap().verification_depth += 1;
        VerificationScope {
            context: self.clone(),
            previous: None,
        }
    }
    /// Whether the caller is in full verification.
    pub fn is_verifying(&self) -> bool {
        self.0.lock().unwrap().verification_depth != 0
    }
    /// Count one hardware fallback, retaining the enclosing algorithmic phase.
    pub fn hardware_fallback(&self) {
        let mut state = self.0.lock().unwrap();
        let phase = state.phase as usize;
        if let Some(report) = &mut state.report {
            report.total.hardware_batch_fallbacks += 1;
            report.phases[phase].hardware_batch_fallbacks += 1;
        }
    }
    /// Count an epoch with unbuffered data, independent of hardware retries.
    pub fn algorithmic_fallback(&self) {
        let mut state = self.0.lock().unwrap();
        if let Some(report) = &mut state.report {
            report.total.algorithmic_fallback_epochs += 1;
            report.phases[VerificationPhase::FallbackData as usize].algorithmic_fallback_epochs +=
                1;
        }
    }
    pub(crate) fn dispatch(
        &self,
        cdb: &[u8],
        buf: &[u8],
        result: &Result<TransferOutcome, ScsiError>,
        duration: Duration,
    ) {
        let command_ms = milliseconds(duration);
        let bucket = [5, 20, 100, 1_000, 10_000].partition_point(|&bound| command_ms >= bound);
        let mut state = self.0.lock().unwrap();
        if state.report.is_none() {
            return;
        }
        let opcode = cdb[0];
        // READ POSITION long form supplies the logical file identifier without
        // another command. MPU means the mark position is unknown.
        if matches!(opcode, 0x01 | 0x11 | 0x2b | 0x91 | 0x92) {
            state.tape_file = None;
        }
        if opcode == 0x34 && cdb.get(1).is_some_and(|action| action & 0x1f == 6) {
            state.tape_file = if matches!(result, Ok(outcome) if outcome.sense.is_none() && outcome.bytes_transferred >= 32)
                && buf.len() >= 32
                && buf[0] & 0x08 == 0
            {
                Some(u64::from_be_bytes(buf[16..24].try_into().unwrap()))
            } else {
                None
            };
        }
        let fixed = opcode == 8 && cdb[1] & 1 != 0;
        let requested = if opcode != 8 {
            0
        } else if fixed {
            u64::from(u32::from_be_bytes([0, cdb[2], cdb[3], cdb[4]]))
        } else {
            1
        };
        let bytes = match result {
            Ok(o) => u64::from(o.bytes_transferred),
            Err(ScsiError::CheckCondition {
                sense,
                bytes_transferred,
            }) if opcode == 8 => {
                let block_size = fixed.then(|| (buf.len() as u64 / requested.max(1)) as u32);
                u64::from(crate::handle::tape_io::transfer_bytes_from_sense(
                    sense,
                    *bytes_transferred,
                    buf.len() as u32,
                    block_size,
                ))
            }
            Err(ScsiError::CheckCondition {
                bytes_transferred, ..
            }) => u64::from(*bytes_transferred),
            _ => 0,
        };
        if opcode == 8 {
            state.read_bytes = state.read_bytes.saturating_add(bytes);
        }
        let recovered = opcode == 8
            && !fixed
            && state.verification_depth > 0
            && exact_recovered(result, buf.len());
        let clean = matches!(result, Ok(o) if o.sense.is_none()) && bytes == buf.len() as u64;
        let confirmed = if clean || recovered { requested } else { 0 };
        let phase = state.phase;
        let before = state.position;
        let protected_end = state.protected_end;
        state.phase_commands[phase as usize] += duration;
        state.command_elapsed += duration;
        let phase_command_ms = milliseconds(state.phase_commands[phase as usize]);
        let total_command_ms = milliseconds(state.command_elapsed);
        let report = state.report.as_mut().unwrap();
        report.total.command_ms = total_command_ms;
        report.phases[phase as usize].command_ms = phase_command_ms;
        for counts in [&mut report.total, &mut report.phases[phase as usize]] {
            counts.max_command_ms = counts.max_command_ms.max(command_ms);
            counts.command_latency[bucket] += 1;
            match opcode {
                0x92 => counts.locate += 1,
                0x11 | 0x91 => counts.space += 1,
                0x34 => counts.read_position += 1,
                8 if fixed => counts.batch_read += 1,
                8 => counts.single_read += 1,
                _ => counts.other += 1,
            }
            counts.records_requested += requested;
            counts.records_confirmed += confirmed;
            counts.bytes += bytes;
            counts.recovered_reads += u64::from(recovered);
        }
        if recovered {
            if let Some(lba) = before {
                report.recovered_lbas.push(lba);
            } else {
                report.recovered_unknown_addresses += 1;
            }
        }
        if opcode == 0x92 && phase == VerificationPhase::ProtectedData {
            let target = u64::from_be_bytes(cdb[4..12].try_into().unwrap());
            if protected_end.is_some_and(|end| target < end) {
                report.backward_data_seeks += 1;
            }
        }
        if opcode == 8 && confirmed > 0 && phase == VerificationPhase::ProtectedData {
            report.protected_bytes += bytes;
            if let Some(start) = before {
                if let Some(end) = start.checked_add(confirmed) {
                    if let Some(last) = report
                        .protected_ranges
                        .last_mut()
                        .filter(|r| r.end_lba == start)
                    {
                        last.end_lba = end;
                    } else {
                        report.protected_ranges.push(IoRange {
                            start_lba: start,
                            end_lba: end,
                        });
                    }
                    state.protected_end = Some(end);
                }
            }
        }
        match opcode {
            8 => {
                state.position = if confirmed > 0 {
                    before.and_then(|p| p.checked_add(confirmed))
                } else {
                    None
                }
            }
            0x34 => {
                state.position = if clean && buf.len() >= 16 && buf[0] & 4 == 0 {
                    Some(u64::from_be_bytes(buf[8..16].try_into().unwrap()))
                } else {
                    None
                }
            }
            0x92 | 0x11 | 0x91 | 1 | 0x1b => state.position = None,
            _ => {}
        }
    }
}
/// Restores the enclosing phase or recovered-read policy on every exit path.
pub struct VerificationScope {
    context: VerificationContext,
    previous: Option<VerificationPhase>,
}
impl Drop for VerificationScope {
    fn drop(&mut self) {
        let now = Instant::now();
        let mut state = self.context.0.lock().unwrap();
        if let Some(previous) = self.previous {
            state.account_phase(now);
            state.phase = previous;
        } else {
            state.verification_depth -= 1;
        }
        // Restoring accounting scope does not enter a new algorithmic phase.
        // The next phase entry or command publishes the actual work performed.
    }
}
/// A report collection lifetime. Dropping also disables collection on errors.
pub struct VerificationIoSession {
    context: VerificationContext,
    started: Instant,
}
impl VerificationIoSession {
    /// Finish collection and return its report.
    pub fn finish(self) -> VerificationIo {
        let now = Instant::now();
        let mut state = self.context.0.lock().unwrap();
        state.account_phase(now);
        let mut report = state.report.take().unwrap();
        report.elapsed_ms = milliseconds(now.duration_since(self.started));
        report.total.elapsed_ms = report.elapsed_ms;
        report
    }
}
impl Drop for VerificationIoSession {
    fn drop(&mut self) {
        self.context.0.lock().unwrap().report = None;
    }
}
impl VerificationContext {
    /// Begin one diagnostic collection before readiness/setup.
    pub(crate) fn begin(&self) -> VerificationIoSession {
        let context = self.clone();
        let started = Instant::now();
        {
            let mut state = context.0.lock().unwrap();
            assert!(state.report.is_none(), "nested verification collectors");
            state.read_bytes = 0;
            state.tape_file = None;
            state.report = Some(VerificationIo::default());
            state.phase = VerificationPhase::Setup;
            state.position = None;
            state.protected_end = None;
            state.phase_started = Some(started);
            state.phase_elapsed = [Duration::ZERO; 7];
            state.phase_commands = [Duration::ZERO; 7];
            state.command_elapsed = Duration::ZERO;
        }
        VerificationIoSession { context, started }
    }
}

/// Accept only current recovered sense with exact bytes and no residual or
/// stream-boundary anomaly. Malformed or deferred sense retains prior handling.
pub(crate) fn exact_recovered(result: &Result<TransferOutcome, ScsiError>, size: usize) -> bool {
    let (sense, bytes) = match result {
        Err(ScsiError::CheckCondition {
            sense,
            bytes_transferred,
        }) => (
            sense.as_slice(),
            crate::handle::tape_io::transfer_bytes_from_sense(
                sense,
                *bytes_transferred,
                size as u32,
                None,
            ),
        ),
        Ok(outcome) => {
            return outcome.bytes_transferred as usize == size
                && size > 0
                && outcome.sense.as_ref().is_some_and(|sense| {
                    sense.key == 1
                        && !sense.ili
                        && !sense.filemark
                        && !sense.eom
                        && sense.information.is_none_or(|r| r == 0)
                })
        }
        _ => return false,
    };
    let Some(decoded) = decode_sense(sense) else {
        return false;
    };
    if size == 0
        || bytes as usize != size
        || decoded.is_deferred()
        || decoded.key != 1
        || decoded.ili
        || decoded.filemark
        || decoded.eom
    {
        return false;
    }
    match decoded.response_code {
        0x70 => {
            sense.len() >= 14
                && sense.len() >= 8 + usize::from(sense[7])
                && (!decoded.valid || sense[3..7] == [0; 4])
        }
        0x72 => {
            if sense.len() < 8 || sense.len() < 8 + usize::from(sense[7]) {
                return false;
            }
            let mut offset = 8;
            let end = 8 + usize::from(sense[7]);
            while offset < end {
                if offset + 2 > end {
                    return false;
                }
                let len = 2 + usize::from(sense[offset + 1]);
                if offset + len > end {
                    return false;
                }
                if sense[offset] == 0
                    && (len < 12
                        || (sense[offset + 2] & 0x80 != 0
                            && sense[offset + 4..offset + 12] != [0; 8]))
                {
                    return false;
                }
                if sense[offset] == 4 && len < 4 {
                    return false;
                }
                offset += len;
            }
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn phase_time_covers_repeated_entries_and_leaves_unused_phases_empty() {
        let context = VerificationContext::default();
        let session = context.begin();
        for _ in 0..2 {
            let _scope = context.phase(VerificationPhase::Walk);
            std::thread::sleep(Duration::from_millis(20));
        }
        let report = session.finish();
        assert!(report.phases[VerificationPhase::Walk as usize].elapsed_ms >= 40);
        for phase in VerificationPhase::ALL.into_iter().skip(2) {
            assert_eq!(report.phases[phase as usize].elapsed_ms, 0);
        }
        assert!(
            report.total.elapsed_ms >= report.phases[VerificationPhase::Walk as usize].elapsed_ms
        );
        assert!(report.total.elapsed_ms <= report.elapsed_ms);
    }

    #[test]
    fn nested_phase_time_is_exclusive_and_finish_accounts_active_scope() {
        let context = VerificationContext::default();
        let session = context.begin();
        let started = Instant::now();
        let outer = context.phase(VerificationPhase::Walk);
        std::thread::sleep(Duration::from_millis(20));
        {
            let _inner = context.phase(VerificationPhase::Sidecar);
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(20));
        let report = session.finish();
        let wall_ms = milliseconds(started.elapsed());
        drop(outer);
        let outer_ms = report.phases[VerificationPhase::Walk as usize].elapsed_ms;
        let inner_ms = report.phases[VerificationPhase::Sidecar as usize].elapsed_ms;
        assert!(outer_ms >= 40);
        assert!(inner_ms >= 20);
        assert!(outer_ms + inner_ms <= wall_ms);
        assert!(report.total.elapsed_ms >= outer_ms + inner_ms);
    }

    #[test]
    fn dispatch_duration_counts_one_bucket_at_each_boundary() {
        for (duration_us, bucket) in [
            (0, 0),
            (4_999, 0),
            (5_000, 1),
            (19_999, 1),
            (20_000, 2),
            (99_999, 2),
            (100_000, 3),
            (999_999, 3),
            (1_000_000, 4),
            (9_999_999, 4),
            (10_000_000, 5),
        ] {
            let context = VerificationContext::default();
            let session = context.begin();
            let duration = Duration::from_micros(duration_us);
            context.dispatch(&[0], &[], &Ok(TransferOutcome::clean(0)), duration);
            let report = session.finish();
            for counts in [
                &report.total,
                &report.phases[VerificationPhase::Setup as usize],
            ] {
                assert!(counts.command_ms >= duration_us / 1000);
                assert!(counts.command_ms < duration_us / 1000 + 1);
                assert!(counts.max_command_ms >= duration_us / 1000);
                assert!(counts.max_command_ms < duration_us / 1000 + 1);
                assert_eq!(counts.command_latency.iter().sum::<u64>(), 1);
                assert_eq!(counts.command_latency[bucket], 1);
            }
        }
    }

    #[test]
    fn command_time_fits_phase_and_retains_fractional_milliseconds() {
        let context = VerificationContext::default();
        let session = context.begin();
        {
            let _scope = context.phase(VerificationPhase::ProtectedData);
            let started = Instant::now();
            std::thread::sleep(Duration::from_millis(20));
            let duration = started.elapsed();
            context.dispatch(&[0], &[], &Ok(TransferOutcome::clean(0)), duration);
            // Short dispatches accumulate without losing each sub-millisecond part.
            std::thread::sleep(Duration::from_millis(3));
            for _ in 0..1500 {
                context.dispatch(
                    &[0],
                    &[],
                    &Ok(TransferOutcome::clean(0)),
                    Duration::from_micros(1),
                );
            }
        }
        let report = session.finish();
        let phase = &report.phases[VerificationPhase::ProtectedData as usize];
        for counts in [phase, &report.total] {
            assert!(counts.command_ms >= 21);
            assert!(counts.command_ms <= counts.elapsed_ms);
            assert!(counts.max_command_ms >= 20);
            assert!(counts.command_ms > counts.max_command_ms);
            assert_eq!(counts.command_latency.iter().sum::<u64>(), 1501);
        }
    }

    fn position(context: &VerificationContext, lba: u64) {
        let mut buf = [0; 32];
        buf[8..16].copy_from_slice(&lba.to_be_bytes());
        context.dispatch(
            &remanence_scsi::read_position::build_cdb_long(),
            &buf,
            &Ok(TransferOutcome::clean(32)),
            Duration::ZERO,
        );
    }
    #[test]
    fn sense_residue_verification_accounting_uses_command_units() {
        for fixed in [false, true] {
            let context = VerificationContext::default();
            let session = context.begin();
            let mut sense = vec![0; 18];
            sense[0] = 0xf0;
            sense[2] = 8;
            sense[3..7].copy_from_slice(&(if fixed { 1u32 } else { 4u32 }).to_be_bytes());
            sense[13] = 5;
            let cdb = if fixed {
                remanence_scsi::read_write::build_read_fixed_cdb(1)
            } else {
                remanence_scsi::read_write::build_read_variable_cdb(4)
            };
            context.dispatch(
                &cdb,
                &[0; 4],
                &Err(ScsiError::CheckCondition {
                    sense,
                    bytes_transferred: 4,
                }),
                Duration::ZERO,
            );
            let report = session.finish();
            assert_eq!(report.total.bytes, 0);
            assert_eq!(report.total.records_confirmed, 0);
        }
    }

    #[test]
    fn nested_scopes_ranges_backward_seeks_and_drop_are_passive() {
        let context = VerificationContext::default();
        let session = context.begin();
        position(&context, 10);
        {
            let _walk = context.phase(VerificationPhase::Walk);
            {
                let _sweep = context.phase(VerificationPhase::ProtectedData);
                context.dispatch(
                    &remanence_scsi::read_write::build_read_variable_cdb(4),
                    &[0; 4],
                    &Ok(TransferOutcome::clean(4)),
                    Duration::ZERO,
                );
                context.dispatch(
                    &remanence_scsi::locate::build_cdb(10),
                    &[],
                    &Ok(TransferOutcome::clean(0)),
                    Duration::ZERO,
                );
                position(&context, 10);
                context.dispatch(
                    &remanence_scsi::read_write::build_read_variable_cdb(4),
                    &[0; 4],
                    &Ok(TransferOutcome::clean(4)),
                    Duration::ZERO,
                );
            }
            position(&context, 30);
        }
        position(&context, 40);
        let report = session.finish();
        assert_eq!(report.backward_data_seeks, 1);
        assert_eq!(report.protected_bytes, 8);
        assert_eq!(
            report.protected_ranges,
            vec![
                IoRange {
                    start_lba: 10,
                    end_lba: 11
                };
                2
            ]
        );
        assert_eq!(
            report.phases[VerificationPhase::Setup as usize].read_position,
            2
        );
        assert_eq!(
            report.phases[VerificationPhase::Walk as usize].read_position,
            1
        );
        assert!(context.0.lock().unwrap().report.is_none());
        {
            let _session = context.begin();
        }
        assert!(context.0.lock().unwrap().report.is_none());
        {
            let _policy = context.verification();
            assert!(context.is_verifying());
        }
        assert!(!context.is_verifying());
    }
    #[test]
    fn recovered_requires_exact_current_consistent_sense() {
        let mut sense = vec![0; 18];
        sense[0] = 0xf0;
        sense[2] = 1;
        sense[7] = 10;
        let check = |sense: Vec<u8>, bytes| {
            exact_recovered(
                &Err(ScsiError::CheckCondition {
                    sense,
                    bytes_transferred: bytes,
                }),
                4,
            )
        };
        assert!(check(sense.clone(), 4));
        assert!(!check(sense.clone(), 3));
        assert!(!check(sense.clone(), 5));
        for (offset, value) in [(0, 0xf1), (2, 0x21), (2, 0x41), (2, 0x81), (6, 1)] {
            let mut bad = sense.clone();
            bad[offset] = value;
            assert!(!check(bad, 4));
        }
        assert!(check(vec![0x72, 1, 0, 0, 0, 0, 0, 0], 4));
        assert!(!check(vec![0x72, 1, 0, 0], 4));
        assert!(!check(
            vec![0x72, 1, 0, 0, 0, 0, 0, 12, 0, 10, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 1],
            4
        ));
    }
}
