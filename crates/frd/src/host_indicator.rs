//! Opt-in desktop "Stop sharing" surface around the canonical host run.
//! One read-only child stays visible while this run is enabled, including idle
//! listening between peers. It must be mapped and responsive BEFORE the host
//! listener starts. It never approves a peer or changes admission policy.
//!
//! The original host runtime stays on its caller's thread and original stack.
//! A scoped liveness watcher checks the native-I/O worker independently. UI
//! loss requests the original host's stop and normal teardown; neither the
//! host future nor its media/firewall cleanup is cancelled and abandoned.
mod process;
pub use process::Error as IndicatorError;
use crate::{
    host_run::{self, Options, Reporter, StopHandle, policy},
    input_process::ProcessLaunch,
};
use process::{Owner, Status};
use std::{
    fmt,
    path::Path,
    sync::{Arc, atomic::{AtomicBool, Ordering}},
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Configuration,
    Entropy,
    Indicator(IndicatorError),
    Host(host_run::Error),
    WatchdogPanicked,
    Cleanup,
}
impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Configuration => "observation_indicator_configuration",
            Self::Entropy => "entropy_unavailable",
            Self::Indicator(IndicatorError::Busy) => "observation_indicator_busy",
            Self::Indicator(IndicatorError::Expired) => "observation_indicator_evidence_lost",
            Self::Indicator(IndicatorError::Protocol) => "observation_indicator_protocol",
            Self::Indicator(_) => "observation_indicator_unavailable",
            Self::Host(error) => error.code(),
            Self::WatchdogPanicked => "observation_indicator_failed",
            Self::Cleanup => "cleanup_incomplete",
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "frd-indicator: {self:?}")
    }
}
impl std::error::Error for Error {}

/// Require the locally installed `fr-observation-indicator` before serving a
/// view-only host, with optional playback audio. A control-capable host retains
/// its separate mandatory per-lease indicator and is refused by this slice.
/// Missing/unresponsive UI refuses rather than silently disabling the option.
pub fn run_with_policy(
    options: &Options,
    report: &Reporter,
    stop: &Arc<StopHandle>,
    policy: policy::Configuration,
    image: &Path,
) -> Result<(), Error> {
    if stop.is_requested() { return Ok(()); }
    if options.input_agent.is_some() { return Err(Error::Configuration); }
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|_| Error::Entropy)?;
    let launch = ProcessLaunch::new(
        image, &options.display, options.xauthority.as_deref(), u128::from_ne_bytes(bytes),
    ).map_err(|_| Error::Configuration)?;
    let stopped = stop.clone();
    let mut owner = Owner::start(launch, move || stopped.request()).map_err(Error::Indicator)?;
    let result = drive(&owner, stop, || host_run::run_with_policy(options, report, stop, policy));
    // Keep the UI through normal host teardown. Joining the I/O worker proves
    // its exact child was reaped; a timeout retains the original worker handle.
    owner.stop();
    let until = Instant::now() + Duration::from_secs(1);
    loop {
        match owner.try_finish() {
            Some(Ok(())) => return result,
            Some(Err(_)) => return Err(Error::Cleanup),
            None if Instant::now() < until => thread::sleep(Duration::from_millis(5)),
            None => return Err(Error::Cleanup),
        }
    }
}
/// Once running, a local UI revocation is a normal stop. Before first readiness
/// EVERY stopped UI is a refusal: an opening failure must not exit as success.
fn fault(status: Status) -> Option<IndicatorError> {
    match status {
        Status::Stopped(IndicatorError::LocalRevoke | IndicatorError::Stopped) => None,
        Status::Stopped(error) => Some(error),
        Status::Opening | Status::Ready => None,
    }
}
fn drive(
    owner: &Owner,
    stop: &StopHandle,
    serve: impl FnOnce() -> Result<(), host_run::Error>,
) -> Result<(), Error> {
    let control = owner.control();
    loop {
        let status = control.status();
        if let Status::Stopped(error) = status { return Err(Error::Indicator(error)); }
        if stop.is_requested() { return Ok(()); }
        if status == Status::Ready { break; }
        thread::sleep(Duration::from_millis(5));
    }
    let done = AtomicBool::new(false);
    thread::scope(|scope| {
        // Unwind stops the ORIGINAL host and releases the watcher before the
        // scope joins it. No runtime coordinator is moved onto a smaller stack.
        let _finish = Finish { done: &done, stop };
        let watcher = scope.spawn(|| {
            // A watcher panic also requests host stop before its thread exits.
            let _fence = Fence(stop);
            let mut failure = None;
            while !done.load(Ordering::Acquire) {
                let status = control.status();
                if let Some(error) = fault(status) { failure.get_or_insert(error); }
                if matches!(status, Status::Stopped(_)) { stop.request(); }
                thread::sleep(Duration::from_millis(5));
            }
            failure
        });
        let result = serve().map_err(Error::Host);
        done.store(true, Ordering::Release);
        let failure = watcher.join().map_err(|_| Error::WatchdogPanicked);
        // Preserve a failed host teardown even if native liveness also failed.
        result?;
        let failure = failure?.or_else(|| fault(control.status()));
        failure.map_or(Ok(()), |error| Err(Error::Indicator(error)))
    })
}
struct Fence<'a>(&'a StopHandle);
impl Drop for Fence<'_> {
    fn drop(&mut self) { self.0.request(); }
}
struct Finish<'a> {
    done: &'a AtomicBool,
    stop: &'a StopHandle,
}
impl Drop for Finish<'_> {
    fn drop(&mut self) {
        self.stop.request();
        self.done.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_failure_is_distinct_from_local_revocation_and_host_cleanup() {
        assert_eq!(fault(Status::Stopped(IndicatorError::LocalRevoke)), None);
        assert_eq!(fault(Status::Stopped(IndicatorError::Stopped)), None);
        assert_eq!(fault(Status::Stopped(IndicatorError::Expired)), Some(IndicatorError::Expired));
        assert_eq!(Error::Indicator(IndicatorError::Expired).code(), "observation_indicator_evidence_lost");
        assert_eq!(Error::Host(host_run::Error::Cleanup("source")).code(), "cleanup_incomplete");
    }
}
