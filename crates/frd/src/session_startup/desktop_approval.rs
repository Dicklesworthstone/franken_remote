//! One original host-startup approval backed by the hardened desktop UI child.
//! Native I/O and child retirement stay on a capacity-one worker. This adapter
//! never creates an Approval or grants input: only its supplied original can
//! consume a result. The enclosing host still services admission and local OS
//! lifecycle BEFORE calling take_decision. This does not enable the CLI by itself.
mod io;
#[cfg(test)]
mod tests;
use super::Approval;
use crate::input_process::ProcessLaunch;
use std::{
    fmt,
    sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const OPEN_BUDGET: Duration = Duration::from_secs(3);
const REPLY_BUDGET: Duration = Duration::from_millis(250);
const MAX_PROMPT: Duration = Duration::from_secs(30);
const TURN: Duration = Duration::from_millis(5);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
static OCCUPIED: AtomicBool = AtomicBool::new(false);
static RETIRED: Mutex<Option<JoinHandle<Report>>> = Mutex::new(None);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Busy,
    Unavailable,
    Protocol,
    Expired,
    Cancelled,
    Consumed,
    Original(super::Error),
    Cleanup,
    Panicked,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "desktop approval: {self:?}") }
}
impl std::error::Error for Error {}
struct Shared {
    original: Approval,
    cancelled: AtomicBool,
    started: Instant,
}
impl Shared {
    fn check(&self) -> Result<(), Error> {
        if self.cancelled.load(Ordering::Acquire) { return Err(Error::Cancelled); }
        // Original expiry/cancellation may be SHORTER than either native bound.
        self.original.check_pending().map_err(Error::Original)?;
        if self.started.elapsed() >= MAX_PROMPT { return Err(Error::Expired); }
        Ok(())
    }
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let _ = self.original.decide(false);
    }
}
/// A native decision and actual cleanup are distinct outcomes. Neither is an
/// observation grant; no answer is consumable until the original worker joins.
#[derive(Clone, Copy)]
struct Report {
    decision: Result<bool, Error>,
    cleanup: Result<(), Error>,
}
#[must_use]
pub struct Prompt {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<Report>>,
    report: Option<Report>,
    consumed: bool,
    permit: bool,
}
impl fmt::Debug for Prompt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DesktopApprovalPrompt")
            .field("consumed", &self.consumed)
            .field("cleanup_collected", &self.report.is_some())
            .finish_non_exhaustive()
    }
}
impl Prompt {
    /// Launch only the locally installed UI image on the already-selected
    /// desktop. The original negotiated role selects view vs control labels.
    /// Validation/spawn failures deny this pending request, never another peer.
    pub fn start(original: Approval, launch: ProcessLaunch) -> Result<Self, Error> {
        original.check_pending().map_err(Error::Original)?;
        if let Err(error) = claim() {
            let _ = original.decide(false);
            return Err(error);
        }
        let shared = Arc::new(Shared {
            original, cancelled: AtomicBool::new(false), started: Instant::now(),
        });
        let running = shared.clone();
        match thread::Builder::new().name("fr-desktop-approval".into()).spawn(move || {
            io::run(&launch, &running)
        }) {
            Ok(worker) => Ok(Self { shared, worker: Some(worker), report: None, consumed: false, permit: true }),
            Err(_) => {
                shared.cancel();
                OCCUPIED.store(false, Ordering::Release);
                Err(Error::Unavailable)
            }
        }
    }
    /// Consume at most one result on the original local session turn. A return
    /// of Some(true) means the ORIGINAL approval accepted the answer, not that
    /// capture started, a decoder is ready, or any input lease was granted.
    pub fn take_decision(&mut self) -> Result<Option<bool>, Error> {
        if self.consumed { return Err(Error::Consumed); }
        if let Err(error) = self.shared.check() {
            self.cancel();
            return Err(error);
        }
        let Some(cleanup) = self.try_finish() else { return Ok(None); };
        self.consumed = true;
        let outcome = cleanup.and_then(|()| self.report.ok_or(Error::Cleanup)?.decision);
        let result = consume(&self.shared, outcome);
        if result.is_err() { self.shared.cancel(); }
        self.release_finished();
        result.map(Some)
    }
    /// Deny and fence immediately. Native cleanup remains separately owned;
    /// this cannot revoke an already-admitted session after approval consumption.
    pub fn cancel(&self) { self.shared.cancel(); }
    /// Only Some(Ok(())) proves the exact native child was reaped and its worker
    /// joined. No decision is applied here, including while collecting after Drop.
    pub fn try_finish(&mut self) -> Option<Result<(), Error>> {
        if let Some(report) = self.report {
            self.release_finished();
            return Some(report.cleanup);
        }
        if self.worker.as_ref().is_some_and(|worker| !worker.is_finished()) { return None; }
        let report = self.worker.take().map_or(
            Report { decision: Err(Error::Cleanup), cleanup: Err(Error::Cleanup) },
            |worker| worker.join().unwrap_or(Report {
                decision: Err(Error::Panicked), cleanup: Err(Error::Panicked),
            }),
        );
        self.report = Some(report);
        self.release_finished();
        Some(report.cleanup)
    }
    fn release_finished(&mut self) {
        // The capacity bound includes a delivered but unconsumed answer, not
        // just native threads. Reaping alone must not open a second prompt.
        if self.permit && self.report.is_some_and(|report| report.cleanup.is_ok())
            && (self.consumed || self.shared.cancelled.load(Ordering::Acquire))
        {
            self.permit = false;
            OCCUPIED.store(false, Ordering::Release);
        }
    }
}
fn consume(shared: &Shared, outcome: Result<bool, Error>) -> Result<bool, Error> {
    // Reaping/native destruction has consumed time too. A delivered yes cannot
    // refresh this budget or retarget a fresh owner with equal numeric IDs.
    shared.check()?;
    let allow = outcome?;
    shared.original.decide(allow).map_err(Error::Original)?;
    Ok(allow)
}
impl Drop for Prompt {
    fn drop(&mut self) {
        self.cancel();
        self.release_finished();
        if let Some(worker) = self.worker.take() {
            let mut retired = RETIRED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            debug_assert!(retired.is_none());
            *retired = Some(worker);
        }
    }
}
fn claim() -> Result<(), Error> {
    let mut retired = RETIRED.lock().map_err(|_| Error::Panicked)?;
    if retired.as_ref().is_some_and(|worker| !worker.is_finished()) { return Err(Error::Busy); }
    if let Some(worker) = retired.take() {
        let report = worker.join().map_err(|_| Error::Panicked)?;
        report.cleanup?;
        // Any unconsumed native yes was already fenced when its owner dropped.
        OCCUPIED.store(false, Ordering::Release);
    }
    OCCUPIED.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ()).map_err(|_| Error::Busy)
}
