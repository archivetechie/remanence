//! Passive, drive-local diagnostics for one full verification. Scopes own shared
//! state rather than borrowing the drive, so nested raw adapters share a report.

use std::sync::{Arc, Mutex};
use std::time::Instant;

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

/// Dispatch counts, including unsuccessful attempts. Bytes include all data-in
/// and data-out transfers; records are exact READ transfers only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IoCounts {
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
    phase: VerificationPhase,
    verification_depth: usize,
    position: Option<u64>,
    protected_end: Option<u64>,
}
/// Shared passive hooks for a drive and its source adapters.
#[derive(Clone, Default)]
pub struct VerificationContext(Arc<Mutex<State>>);
impl VerificationContext {
    /// Enter a phase without issuing commands.
    pub fn phase(&self, phase: VerificationPhase) -> VerificationScope {
        let mut state = self.0.lock().unwrap();
        let previous = state.phase;
        state.phase = phase;
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
    ) {
        let mut state = self.0.lock().unwrap();
        if state.report.is_none() {
            return;
        }
        let opcode = cdb[0];
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
                bytes_transferred, ..
            }) => u64::from(*bytes_transferred),
            _ => 0,
        };
        let recovered = opcode == 8
            && !fixed
            && state.verification_depth > 0
            && exact_recovered(result, buf.len());
        let clean = matches!(result, Ok(o) if o.sense.is_none()) && bytes == buf.len() as u64;
        let confirmed = if clean || recovered { requested } else { 0 };
        let phase = state.phase;
        let before = state.position;
        let protected_end = state.protected_end;
        let report = state.report.as_mut().unwrap();
        for counts in [&mut report.total, &mut report.phases[phase as usize]] {
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
        let mut state = self.context.0.lock().unwrap();
        if let Some(previous) = self.previous {
            state.phase = previous;
        } else {
            state.verification_depth -= 1;
        }
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
        let mut report = self.context.0.lock().unwrap().report.take().unwrap();
        report.elapsed_ms = self
            .started
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
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
        {
            let mut state = context.0.lock().unwrap();
            assert!(state.report.is_none(), "nested verification collectors");
            state.report = Some(VerificationIo::default());
            state.phase = VerificationPhase::Setup;
            state.position = None;
            state.protected_end = None;
        }
        VerificationIoSession {
            context,
            started: Instant::now(),
        }
    }
}

/// Accept only current recovered sense with exact bytes and no residual or
/// stream-boundary anomaly. Malformed or deferred sense retains prior handling.
pub(crate) fn exact_recovered(result: &Result<TransferOutcome, ScsiError>, size: usize) -> bool {
    let (sense, bytes) = match result {
        Err(ScsiError::CheckCondition {
            sense,
            bytes_transferred,
        }) => (sense.as_slice(), *bytes_transferred),
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
    fn position(context: &VerificationContext, lba: u64) {
        let mut buf = [0; 32];
        buf[8..16].copy_from_slice(&lba.to_be_bytes());
        context.dispatch(
            &remanence_scsi::read_position::build_cdb_long(),
            &buf,
            &Ok(TransferOutcome::clean(32)),
        );
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
                );
                context.dispatch(
                    &remanence_scsi::locate::build_cdb(10),
                    &[],
                    &Ok(TransferOutcome::clean(0)),
                );
                position(&context, 10);
                context.dispatch(
                    &remanence_scsi::read_write::build_read_variable_cdb(4),
                    &[0; 4],
                    &Ok(TransferOutcome::clean(4)),
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
