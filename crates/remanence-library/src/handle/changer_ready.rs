//! Bounded wait-and-retry for transient changer not-ready conditions on
//! MOVE MEDIUM.
//!
//! A shared library's operator routinely opens and closes a magazine or the
//! door while Remanence works. A MOVE MEDIUM issued in that window fails with
//! CHECK CONDITION and is *not executed*: the cartridge is still at its source.
//! [`ChangerHandle::move_medium`](super::ChangerHandle::move_medium) can ride
//! the window out instead of failing the client operation:
//!
//! 1. Classify the failure. Only the senses in the table below start a wait;
//!    everything else fails exactly as before.
//! 2. Poll the changer with TEST UNIT READY. Once it answers GOOD, read
//!    element status and confirm that the move's source and destination
//!    report no element exception and are accessible. Polls are spaced by
//!    [`ChangerReadyWait::poll_interval`] and bounded by
//!    [`ChangerReadyWait::timeout`], measured from the first failure.
//! 3. When ready, run the move's normal element check against the inventory
//!    from before the failed attempt. The move is retried only if the source
//!    still holds the same cartridge (occupancy and volume tag) and the
//!    destination is still empty. Any change refuses with the refreshed
//!    inventory, as a failed move did before. At most
//!    [`ChangerReadyWait::max_retries`] retries follow the first attempt.
//!
//! | Sense | Name (SPC-4 / SMC-3) | Starts a wait | On the retry path |
//! | --- | --- | --- | --- |
//! | `02/3B/12` | MEDIUM MAGAZINE REMOVED | yes | yes |
//! | `02/04/01` | LOGICAL UNIT IS IN PROCESS OF BECOMING READY | yes | yes |
//! | `02/04/18` | LOGICAL UNIT NOT READY, A DOOR IS OPEN | yes | yes |
//! | `06/28/00` | NOT READY TO READY CHANGE, MEDIUM MAY HAVE CHANGED | no | yes |
//! | `06/29/xx` | POWER ON, RESET, OR BUS DEVICE RESET OCCURRED | no | yes |
//! | `06/3B/12` | MEDIUM MAGAZINE REMOVED (unit attention) | no | yes |
//! | `06/3B/13` | MEDIUM MAGAZINE INSERTED (unit attention) | no | yes |
//!
//! Unit attentions are consumed by the command that reports them, so on the
//! retry path they only mean "poll again". Every other sense keeps its
//! existing handling. That includes `02/04/02` (initializing command
//! required), `02/04/03` (manual intervention required) and the
//! offline/robotics-disabled/configuration qualifiers: time alone does not
//! clear them, and the daemon never issues INITIALIZE ELEMENT STATUS on its
//! own. ILLEGAL REQUEST, hardware and medium errors, BUSY and transport
//! failures also keep their handling.
//!
//! Only current sense (fixed `0x70` or descriptor `0x72`) is classified;
//! deferred errors (`0x71`/`0x73`) report an earlier command and never start
//! a wait.
//!
//! A move is never retried when the source's volume tag was unknown before
//! the move: an unlabelled cartridge swapped in while the magazine was out
//! would be indistinguishable from the original. Such a move fails at once.
//! When fresh element status before the first attempt already shows the
//! source or destination unavailable with a transient element exception, the
//! wait starts there instead of first issuing a MOVE that is bound to fail.
//! That pre-move wait counts as the first wait against
//! [`ChangerReadyWait::max_retries`], and the MOVE after it is classified as a
//! retry, so a unit attention on it is waited out too. If the pre-move wait
//! is stopped or reaches its deadline, no MOVE is issued and the move fails
//! with [`crate::MoveError::NotReady`] after a refresh. If it ends because a
//! poll answered with something waiting does not clear, the endpoints are
//! re-read and compared as after a successful wait before the one MOVE that
//! follows. A stop signal already set on entry spends no time, and the MOVE
//! is issued as it would be without the wait.
//!
//! A wait stops early, and the move fails as before, when any stop signal
//! given by [`ChangerReadyWait::with_stop_signal`] is set (daemon shutdown,
//! an operation's cancellation, a caller that has gone away). A target that
//! answers BUSY or TASK SET FULL is polled again, up to
//! [`CHANGER_BUSY_POLLS`] consecutive times.
//!
//! The wait holds no lock while sleeping. Callers that own a drive or bay
//! reservation keep it across the wait: the cartridge has not moved, so the
//! reservation still describes where it is.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use remanence_scsi::read_element_status::{ElementStatusData, ElementType};
use remanence_scsi::ScsiError;

use super::ChangerHandle;
use crate::transport::TimeoutClass;

/// Default bound for one MOVE MEDIUM's not-ready wait in the daemon.
pub const DEFAULT_CHANGER_NOT_READY_WAIT: Duration = Duration::from_secs(600);
/// Default spacing between changer readiness polls.
pub const DEFAULT_CHANGER_NOT_READY_POLL: Duration = Duration::from_secs(5);
/// Default spacing between "still waiting" progress reports.
pub const DEFAULT_CHANGER_NOT_READY_PROGRESS: Duration = Duration::from_secs(60);
/// Default number of MOVE MEDIUM retries after the first attempt.
pub const DEFAULT_CHANGER_NOT_READY_RETRIES: u32 = 3;
/// Longest wait a [`ChangerReadyWait`] accepts; longer timeouts are clamped.
pub const MAX_CHANGER_NOT_READY_WAIT: Duration = Duration::from_secs(3600);
/// Shortest accepted poll interval, so a misconfigured wait cannot spin.
pub const MIN_CHANGER_NOT_READY_POLL: Duration = Duration::from_millis(100);
/// Consecutive BUSY / TASK SET FULL poll answers tolerated before giving up.
pub const CHANGER_BUSY_POLLS: u32 = 3;
/// Longest single sleep between stop-signal checks.
const STOP_CHECK_SLICE: Duration = Duration::from_millis(100);

/// Observer for [`ChangerWaitEvent`]s, for example a live-status board.
pub type ChangerWaitObserver = Arc<dyn Fn(&ChangerWaitEvent<'_>) + Send + Sync>;

/// How a changer handle waits out a transient not-ready MOVE MEDIUM failure.
///
/// The default is disabled: a zero timeout fails at once, which keeps the
/// library-level behaviour for direct callers. The daemon enables it from
/// `[daemon] changer_not_ready_wait_seconds`.
#[derive(Clone)]
pub struct ChangerReadyWait {
    /// Longest total wait for one MOVE MEDIUM, from its first failure.
    /// Zero disables the wait.
    pub timeout: Duration,
    /// Spacing between readiness polls.
    pub poll_interval: Duration,
    /// Spacing between "still waiting" progress reports.
    pub progress_interval: Duration,
    /// MOVE MEDIUM retries after the first attempt.
    pub max_retries: u32,
    observer: Option<ChangerWaitObserver>,
    stops: Vec<(&'static str, Arc<AtomicBool>)>,
}

impl Default for ChangerReadyWait {
    fn default() -> Self {
        Self::disabled()
    }
}

impl std::fmt::Debug for ChangerReadyWait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChangerReadyWait")
            .field("timeout", &self.timeout)
            .field("poll_interval", &self.poll_interval)
            .field("progress_interval", &self.progress_interval)
            .field("max_retries", &self.max_retries)
            .field("observer", &self.observer.as_ref().map(|_| "<observer>"))
            .field(
                "stops",
                &self
                    .stops
                    .iter()
                    .map(|(reason, _)| *reason)
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl ChangerReadyWait {
    /// No wait: a transient not-ready failure fails the move at once.
    pub fn disabled() -> Self {
        Self::bounded(Duration::ZERO)
    }

    /// Wait up to `timeout` (at most [`MAX_CHANGER_NOT_READY_WAIT`]) with
    /// the default poll, progress and retry settings.
    pub fn bounded(timeout: Duration) -> Self {
        Self {
            timeout: timeout.min(MAX_CHANGER_NOT_READY_WAIT),
            poll_interval: DEFAULT_CHANGER_NOT_READY_POLL,
            progress_interval: DEFAULT_CHANGER_NOT_READY_PROGRESS,
            max_retries: DEFAULT_CHANGER_NOT_READY_RETRIES,
            observer: None,
            stops: Vec::new(),
        }
    }

    /// Replace the poll interval, at least [`MIN_CHANGER_NOT_READY_POLL`].
    pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval.max(MIN_CHANGER_NOT_READY_POLL);
        self
    }

    /// Replace the progress-report interval, at most
    /// [`MAX_CHANGER_NOT_READY_WAIT`] so the next report time cannot overflow.
    pub fn with_progress_interval(mut self, progress_interval: Duration) -> Self {
        self.progress_interval =
            progress_interval.clamp(MIN_CHANGER_NOT_READY_POLL, MAX_CHANGER_NOT_READY_WAIT);
        self
    }

    /// Replace the retry limit.
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Report wait start, progress and end to `observer`.
    pub fn with_observer(mut self, observer: ChangerWaitObserver) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Stop waiting, and fail the move as before, once `flag` is set.
    /// `reason` names the signal in logs, for example `daemon shutdown`.
    pub fn with_stop_signal(mut self, reason: &'static str, flag: Arc<AtomicBool>) -> Self {
        self.stops.push((reason, flag));
        self
    }

    /// The first stop signal that is set, if any.
    pub fn stop_reason(&self) -> Option<&'static str> {
        self.stops
            .iter()
            .find(|(_, flag)| flag.load(Ordering::SeqCst))
            .map(|(reason, _)| *reason)
    }

    /// True when a transient not-ready failure may be waited out.
    pub fn is_enabled(&self) -> bool {
        !self.timeout.is_zero() && self.max_retries > 0
    }

    fn notify(&self, event: &ChangerWaitEvent<'_>) {
        if let Some(observer) = &self.observer {
            observer(event);
        }
    }
}

/// Why a changer is not ready for a MOVE MEDIUM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangerNotReady {
    /// The changer answered CHECK CONDITION with this sense.
    Sense {
        /// Sense key.
        key: u8,
        /// Additional sense code.
        asc: u8,
        /// Additional sense code qualifier.
        ascq: u8,
    },
    /// READ ELEMENT STATUS reports one of the move's elements unavailable.
    /// `asc`/`ascq` are the element exception; both zero when only the
    /// ACCESS bit is clear.
    Element {
        /// Element address.
        address: u16,
        /// Element exception ASC.
        asc: u8,
        /// Element exception ASCQ.
        ascq: u8,
    },
}

impl ChangerNotReady {
    /// Operator-facing reason, for example `medium magazine removed`.
    pub fn reason(&self) -> &'static str {
        let (asc, ascq) = match *self {
            Self::Sense { asc, ascq, .. } => (asc, ascq),
            Self::Element {
                asc: 0, ascq: 0, ..
            } => return "element not accessible",
            Self::Element { asc, ascq, .. } => (asc, ascq),
        };
        changer_sense_name(asc, ascq).unwrap_or("not ready")
    }
}

impl std::fmt::Display for ChangerNotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::Sense { key, asc, ascq } => {
                write!(f, "{} ({key:02x}/{asc:02x}/{ascq:02x})", self.reason())
            }
            Self::Element {
                address,
                asc: 0,
                ascq: 0,
            } => write!(f, "element 0x{address:04x} not accessible"),
            Self::Element { address, asc, ascq } => write!(
                f,
                "element 0x{address:04x} {} ({asc:02x}/{ascq:02x})",
                self.reason()
            ),
        }
    }
}

/// Where a changer readiness wait stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangerWaitPhase {
    /// A MOVE MEDIUM failed transiently; the wait starts.
    Waiting,
    /// Periodic progress while the condition persists.
    StillWaiting,
    /// The changer and both elements are ready; the move is re-checked.
    Ready,
    /// The wait ends without readiness: timeout or a non-transient answer.
    GaveUp,
}

/// One changer readiness wait report.
#[derive(Debug, Clone, Copy)]
pub struct ChangerWaitEvent<'a> {
    /// Library whose changer is not ready.
    pub library_serial: &'a str,
    /// Source element of the waiting move.
    pub src: u16,
    /// Destination element of the waiting move.
    pub dst: u16,
    /// Where the wait stands.
    pub phase: ChangerWaitPhase,
    /// Most recent not-ready reason.
    pub condition: ChangerNotReady,
    /// Time since the move's first transient failure.
    pub elapsed: Duration,
    /// Configured bound on the wait.
    pub timeout: Duration,
}

/// SPC/SMC name of a changer readiness sense, in lower case.
pub fn changer_sense_name(asc: u8, ascq: u8) -> Option<&'static str> {
    Some(match (asc, ascq) {
        (0x04, 0x01) => "becoming ready",
        (0x04, 0x18) => "a door is open",
        (0x28, 0x00) => "not ready to ready change, medium may have changed",
        (0x29, _) => "power on, reset, or bus device reset occurred",
        (0x3b, 0x12) => "medium magazine removed",
        (0x3b, 0x13) => "medium magazine inserted",
        _ => return None,
    })
}

fn decoded(err: &ScsiError) -> Option<(u8, u8, u8)> {
    let sense = match err {
        #[cfg(target_os = "linux")]
        ScsiError::CheckCondition { sense, .. } => sense,
        _ => return None,
    };
    let sense = remanence_scsi::decode_sense(sense)?;
    // Deferred sense reports an earlier command, not this one's readiness.
    if !(sense.is_fixed_format() || sense.is_descriptor_format()) {
        return None;
    }
    Some((sense.key, sense.asc, sense.ascq))
}

fn not_ready_condition(key: u8, asc: u8, ascq: u8) -> bool {
    key == 0x02 && matches!((asc, ascq), (0x3b, 0x12) | (0x04, 0x01) | (0x04, 0x18))
}

fn ready_transition(key: u8, asc: u8, ascq: u8) -> bool {
    key == 0x06 && matches!((asc, ascq), (0x28, 0x00) | (0x29, _) | (0x3b, 0x12 | 0x13))
}

/// Classify a failed MOVE MEDIUM's first attempt. `Some` only for the
/// not-ready senses that start a wait; see the module table.
pub fn classify_changer_not_ready(err: &ScsiError) -> Option<ChangerNotReady> {
    let (key, asc, ascq) = decoded(err)?;
    not_ready_condition(key, asc, ascq).then_some(ChangerNotReady::Sense { key, asc, ascq })
}

/// Classify a changer answer once a wait has started: the not-ready senses
/// plus the unit attentions a changer reports as it becomes ready again.
pub fn classify_changer_ready_transition(err: &ScsiError) -> Option<ChangerNotReady> {
    let (key, asc, ascq) = decoded(err)?;
    (not_ready_condition(key, asc, ascq) || ready_transition(key, asc, ascq))
        .then_some(ChangerNotReady::Sense { key, asc, ascq })
}

/// Element-status exceptions that mark an element temporarily unavailable
/// rather than failed: an open magazine, a library becoming ready, an open
/// door.
pub fn is_transient_element_exception(asc: u8, ascq: u8) -> bool {
    matches!((asc, ascq), (0x3b, 0x12) | (0x04, 0x01) | (0x04, 0x18))
}

/// Result of one readiness poll.
enum Probe {
    Ready,
    Waiting(ChangerNotReady),
    /// The target answered BUSY or TASK SET FULL.
    Busy(String),
    /// The changer answered with something waiting will not clear.
    Unavailable(String),
}

/// How a wait ended.
pub(super) enum WaitOutcome {
    Ready,
    GaveUp(GaveUp),
}

/// Why a wait ended without readiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GaveUp {
    /// A stop signal was already set; no time was spent.
    StoppedOnEntry,
    /// A stop signal was set while waiting.
    Stopped,
    /// The deadline passed.
    Deadline,
    /// A poll answered with something waiting does not clear, or the target
    /// stayed busy.
    Unavailable,
}

/// Element readiness for one move endpoint in fresh element status.
fn element_probe(status: &ElementStatusData, address: u16) -> Result<(), Probe> {
    let Some(element) = status
        .elements
        .iter()
        .find(|element| element.address == address)
    else {
        return Err(Probe::Unavailable(format!(
            "element 0x{address:04x} is missing from element status"
        )));
    };
    if element.except {
        if is_transient_element_exception(element.asc, element.ascq) {
            return Err(Probe::Waiting(ChangerNotReady::Element {
                address,
                asc: element.asc,
                ascq: element.ascq,
            }));
        }
        return Err(Probe::Unavailable(format!(
            "element 0x{address:04x} reports exception {:02x}/{:02x}",
            element.asc, element.ascq
        )));
    }
    // A data transfer element's ACCESS bit follows the drive's own state
    // (a threaded cartridge reads as inaccessible), so only storage and
    // import/export elements wait on it.
    if !element.access && !matches!(element.element_type, ElementType::DataTransfer) {
        return Err(Probe::Waiting(ChangerNotReady::Element {
            address,
            asc: 0,
            ascq: 0,
        }));
    }
    Ok(())
}

/// Classify a failed readiness poll command.
fn poll_failure(command: &str, err: &ScsiError) -> Probe {
    if let Some(condition) = classify_changer_ready_transition(err) {
        return Probe::Waiting(condition);
    }
    #[cfg(target_os = "linux")]
    if let ScsiError::UnexpectedStatus {
        status: 0x08 | 0x28,
    } = err
    {
        return Probe::Busy(format!("{command}: {err}"));
    }
    Probe::Unavailable(format!("{command}: {err}"))
}

/// A transient element exception on `address` in the handle's inventory.
fn inventory_exception(library: &crate::model::Library, address: u16) -> Option<ChangerNotReady> {
    let exception = library
        .drive_bays
        .iter()
        .find(|bay| bay.element_address == address)
        .map(|bay| bay.exception)
        .or_else(|| {
            library
                .slots
                .iter()
                .find(|slot| slot.element_address == address)
                .map(|slot| slot.exception)
        })
        .or_else(|| {
            library
                .ie_ports
                .iter()
                .find(|port| port.element_address == address)
                .map(|port| port.exception)
        })
        .flatten()?;
    is_transient_element_exception(exception.asc, exception.ascq).then_some(
        ChangerNotReady::Element {
            address,
            asc: exception.asc,
            ascq: exception.ascq,
        },
    )
}

/// Compact operator-facing duration: `750ms`, `45s`, `10m`, `2m30s`.
pub(crate) fn human_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs == 0 {
        return format!("{}ms", duration.as_millis());
    }
    match (secs / 60, secs % 60) {
        (0, s) => format!("{s}s"),
        (m, 0) => format!("{m}m"),
        (m, s) => format!("{m}m{s}s"),
    }
}

impl ChangerHandle {
    /// Replace how this handle waits out transient not-ready MOVE failures.
    pub fn set_ready_wait(&mut self, wait: ChangerReadyWait) {
        self.ready_wait = wait;
    }

    /// How this handle waits out transient not-ready MOVE failures.
    pub fn ready_wait(&self) -> &ChangerReadyWait {
        &self.ready_wait
    }

    /// Transient classification of a failed MOVE attempt, if a wait may start
    /// or continue for it. Retries also accept the ready-transition unit
    /// attentions.
    pub(super) fn transient_move_failure(
        &self,
        err: &ScsiError,
        retries: u32,
    ) -> Option<ChangerNotReady> {
        if !self.ready_wait.is_enabled() || retries >= self.ready_wait.max_retries {
            return None;
        }
        if retries == 0 {
            classify_changer_not_ready(err)
        } else {
            classify_changer_ready_transition(err)
        }
    }

    /// TEST UNIT READY, then element status for `src` and `dst`. The element
    /// status is only inspected: the move's own check re-reads and compares it.
    fn probe_ready(&mut self, src: u16, dst: u16) -> Probe {
        let tur = {
            let mut transport = self.transport.lock().unwrap_or_else(|e| e.into_inner());
            transport.set_timeout_for(TimeoutClass::Inquiry);
            transport.execute_none(&[0u8; 6])
        };
        if let Err(err) = tur {
            return poll_failure("TEST UNIT READY", &err);
        }
        let status = crate::discovery::issue_res(
            self.transport
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_mut(),
            0,
            true,
            true,
        );
        let status = match status {
            Ok(status) => status,
            Err(err) => return poll_failure("READ ELEMENT STATUS", &err),
        };
        for address in [src, dst] {
            if let Err(probe) = element_probe(&status, address) {
                return probe;
            }
        }
        Probe::Ready
    }

    /// A transient exception the handle's fresh inventory already shows on
    /// the move's source or destination, before any MOVE is issued.
    pub(super) fn endpoint_unavailable(&self, src: u16, dst: u16) -> Option<ChangerNotReady> {
        inventory_exception(&self.library, src).or_else(|| inventory_exception(&self.library, dst))
    }

    /// Poll until the changer and both move elements are ready, the deadline
    /// passes, a stop signal is set, or the changer answers with something
    /// waiting will not clear.
    pub(super) fn wait_for_ready(
        &mut self,
        src: u16,
        dst: u16,
        mut condition: ChangerNotReady,
        first_failure: Instant,
        attempt: u32,
    ) -> WaitOutcome {
        let mut wait = self.ready_wait.clone();
        // The fields are public; apply the builder's bounds here as well.
        wait.poll_interval = wait.poll_interval.max(MIN_CHANGER_NOT_READY_POLL);
        wait.progress_interval = wait
            .progress_interval
            .clamp(MIN_CHANGER_NOT_READY_POLL, MAX_CHANGER_NOT_READY_WAIT);
        let serial = self.library.serial.clone();
        let deadline = first_failure
            .checked_add(wait.timeout.min(MAX_CHANGER_NOT_READY_WAIT))
            .unwrap_or_else(Instant::now);
        let event = |phase, condition, elapsed| ChangerWaitEvent {
            library_serial: serial.as_str(),
            src,
            dst,
            phase,
            condition,
            elapsed,
            timeout: wait.timeout,
        };
        if let Some(reason) = wait.stop_reason() {
            tracing::warn!(
                library_serial = %serial,
                %condition,
                "library {serial} not ready ({}); not waiting ({reason}); MOVE MEDIUM 0x{src:04x} -> 0x{dst:04x} fails",
                condition.reason(),
            );
            return WaitOutcome::GaveUp(GaveUp::StoppedOnEntry);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        tracing::warn!(
            library_serial = %serial,
            src = format_args!("0x{src:04x}"),
            dst = format_args!("0x{dst:04x}"),
            %condition,
            attempt = attempt + 1,
            "library {serial} not ready ({}); waiting up to {} before retrying MOVE MEDIUM 0x{src:04x} -> 0x{dst:04x}",
            condition.reason(),
            human_duration(remaining),
        );
        wait.notify(&event(
            ChangerWaitPhase::Waiting,
            condition,
            first_failure.elapsed(),
        ));
        let give_up = |why: GaveUp, detail: String, condition| {
            tracing::warn!(
                library_serial = %serial,
                %condition,
                "library {serial} {detail}; MOVE MEDIUM 0x{src:04x} -> 0x{dst:04x} fails",
            );
            wait.notify(&event(
                ChangerWaitPhase::GaveUp,
                condition,
                first_failure.elapsed(),
            ));
            WaitOutcome::GaveUp(why)
        };
        let mut next_progress = Instant::now() + wait.progress_interval;
        let mut busy_polls = 0u32;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return give_up(
                    GaveUp::Deadline,
                    format!(
                        "still not ready ({}) after {}",
                        condition.reason(),
                        human_duration(first_failure.elapsed())
                    ),
                    condition,
                );
            }
            // Sleep in short slices so a stop signal ends the wait promptly.
            let wake = now + wait.poll_interval.min(deadline - now);
            loop {
                if let Some(reason) = wait.stop_reason() {
                    return give_up(
                        GaveUp::Stopped,
                        format!("readiness wait stops ({reason})"),
                        condition,
                    );
                }
                let now = Instant::now();
                if now >= wake {
                    break;
                }
                std::thread::sleep((wake - now).min(STOP_CHECK_SLICE));
            }
            match self.probe_ready(src, dst) {
                Probe::Ready => {
                    tracing::warn!(
                        library_serial = %serial,
                        "library {serial} ready after {}; re-checking 0x{src:04x} and 0x{dst:04x} before retrying MOVE MEDIUM",
                        human_duration(first_failure.elapsed()),
                    );
                    wait.notify(&event(
                        ChangerWaitPhase::Ready,
                        condition,
                        first_failure.elapsed(),
                    ));
                    return WaitOutcome::Ready;
                }
                Probe::Waiting(now_condition) => {
                    busy_polls = 0;
                    if now_condition != condition {
                        tracing::info!(
                            library_serial = %serial,
                            previous = %condition,
                            condition = %now_condition,
                            "library {serial} readiness changes while waiting",
                        );
                        condition = now_condition;
                    }
                }
                Probe::Busy(detail) => {
                    busy_polls += 1;
                    if busy_polls > CHANGER_BUSY_POLLS {
                        return give_up(
                            GaveUp::Unavailable,
                            format!("stays busy for {busy_polls} polls ({detail})"),
                            condition,
                        );
                    }
                    tracing::info!(library_serial = %serial, %detail, busy_polls, "library {serial} busy while waiting");
                }
                Probe::Unavailable(detail) => {
                    return give_up(
                        GaveUp::Unavailable,
                        format!("answers a readiness poll with a condition waiting does not clear ({detail})"),
                        condition,
                    );
                }
            }
            if Instant::now() >= next_progress {
                tracing::warn!(
                    library_serial = %serial,
                    %condition,
                    "library {serial} still not ready ({}) after {}; waiting up to {} more",
                    condition.reason(),
                    human_duration(first_failure.elapsed()),
                    human_duration(deadline.saturating_duration_since(Instant::now())),
                );
                wait.notify(&event(
                    ChangerWaitPhase::StillWaiting,
                    condition,
                    first_failure.elapsed(),
                ));
                next_progress += wait.progress_interval;
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn check(key: u8, asc: u8, ascq: u8) -> ScsiError {
        let mut sense = vec![0u8; 18];
        sense[0] = 0x70;
        sense[2] = key;
        sense[7] = 10;
        sense[12] = asc;
        sense[13] = ascq;
        ScsiError::CheckCondition {
            sense,
            bytes_transferred: 0,
        }
    }

    #[test]
    fn first_attempt_waits_only_for_not_ready_senses() {
        for (key, asc, ascq) in [(0x02, 0x3b, 0x12), (0x02, 0x04, 0x01), (0x02, 0x04, 0x18)] {
            assert_eq!(
                classify_changer_not_ready(&check(key, asc, ascq)),
                Some(ChangerNotReady::Sense { key, asc, ascq }),
                "{key:02x}/{asc:02x}/{ascq:02x}"
            );
        }
        for (key, asc, ascq) in [
            (0x05, 0x3b, 0x0e),
            (0x05, 0x3b, 0x0d),
            (0x05, 0x24, 0x00),
            (0x04, 0x15, 0x01),
            (0x03, 0x31, 0x00),
            (0x02, 0x04, 0x00),
            (0x02, 0x04, 0x02),
            (0x02, 0x04, 0x03),
            (0x02, 0x04, 0x12),
            (0x02, 0x3a, 0x00),
            (0x06, 0x28, 0x00),
            (0x06, 0x29, 0x00),
            (0x06, 0x3b, 0x13),
            (0x05, 0x53, 0x02),
        ] {
            assert_eq!(
                classify_changer_not_ready(&check(key, asc, ascq)),
                None,
                "{key:02x}/{asc:02x}/{ascq:02x}"
            );
        }
        assert_eq!(
            classify_changer_not_ready(&ScsiError::TransportError {
                status: 0,
                host_status: 3,
                driver_status: 0,
                info: 1,
                sense: Vec::new(),
            }),
            None
        );
    }

    #[test]
    fn only_current_sense_starts_a_wait() {
        let mut descriptor = vec![0u8; 8];
        descriptor[0] = 0x72;
        descriptor[1] = 0x02;
        descriptor[2] = 0x3b;
        descriptor[3] = 0x12;
        let current = ScsiError::CheckCondition {
            sense: descriptor.clone(),
            bytes_transferred: 0,
        };
        assert!(classify_changer_not_ready(&current).is_some());
        for code in [0x71u8, 0x73] {
            let mut sense = if code == 0x71 {
                match check(0x02, 0x3b, 0x12) {
                    ScsiError::CheckCondition { sense, .. } => sense,
                    _ => unreachable!(),
                }
            } else {
                descriptor.clone()
            };
            sense[0] = code;
            let deferred = ScsiError::CheckCondition {
                sense,
                bytes_transferred: 0,
            };
            assert_eq!(classify_changer_not_ready(&deferred), None, "{code:02x}");
            assert_eq!(
                classify_changer_ready_transition(&deferred),
                None,
                "{code:02x}"
            );
        }
    }

    #[test]
    fn timeouts_are_clamped_and_stop_signals_named() {
        use std::sync::atomic::AtomicBool;
        let wait = ChangerReadyWait::bounded(Duration::from_secs(u64::MAX));
        assert_eq!(wait.timeout, MAX_CHANGER_NOT_READY_WAIT);
        let wait = wait
            .with_poll_interval(Duration::ZERO)
            .with_progress_interval(Duration::MAX);
        assert_eq!(wait.poll_interval, MIN_CHANGER_NOT_READY_POLL);
        assert_eq!(wait.progress_interval, MAX_CHANGER_NOT_READY_WAIT);
        let flag = Arc::new(AtomicBool::new(false));
        let wait = wait.with_stop_signal("daemon shutdown", Arc::clone(&flag));
        assert_eq!(wait.stop_reason(), None);
        flag.store(true, Ordering::SeqCst);
        assert_eq!(wait.stop_reason(), Some("daemon shutdown"));
    }

    #[test]
    fn retry_path_also_consumes_ready_transition_unit_attentions() {
        for (key, asc, ascq) in [
            (0x02, 0x3b, 0x12),
            (0x06, 0x28, 0x00),
            (0x06, 0x29, 0x00),
            (0x06, 0x29, 0x02),
            (0x06, 0x3b, 0x12),
            (0x06, 0x3b, 0x13),
        ] {
            assert!(
                classify_changer_ready_transition(&check(key, asc, ascq)).is_some(),
                "{key:02x}/{asc:02x}/{ascq:02x}"
            );
        }
        for (key, asc, ascq) in [(0x06, 0x28, 0x01), (0x06, 0x2a, 0x01), (0x05, 0x3b, 0x0e)] {
            assert!(
                classify_changer_ready_transition(&check(key, asc, ascq)).is_none(),
                "{key:02x}/{asc:02x}/{ascq:02x}"
            );
        }
    }

    #[test]
    fn conditions_name_the_reason_and_the_element() {
        let removed = ChangerNotReady::Sense {
            key: 0x02,
            asc: 0x3b,
            ascq: 0x12,
        };
        assert_eq!(removed.to_string(), "medium magazine removed (02/3b/12)");
        let element = ChangerNotReady::Element {
            address: 0x03ec,
            asc: 0x3b,
            ascq: 0x12,
        };
        assert_eq!(
            element.to_string(),
            "element 0x03ec medium magazine removed (3b/12)"
        );
        let hidden = ChangerNotReady::Element {
            address: 0x03ec,
            asc: 0,
            ascq: 0,
        };
        assert_eq!(hidden.to_string(), "element 0x03ec not accessible");
        assert_eq!(human_duration(Duration::from_secs(600)), "10m");
        assert_eq!(human_duration(Duration::from_secs(150)), "2m30s");
        assert_eq!(human_duration(Duration::from_millis(40)), "40ms");
    }

    #[test]
    fn default_wait_is_disabled() {
        assert!(!ChangerReadyWait::default().is_enabled());
        assert!(ChangerReadyWait::bounded(DEFAULT_CHANGER_NOT_READY_WAIT).is_enabled());
        assert!(!ChangerReadyWait::bounded(DEFAULT_CHANGER_NOT_READY_WAIT)
            .with_max_retries(0)
            .is_enabled());
    }
}
