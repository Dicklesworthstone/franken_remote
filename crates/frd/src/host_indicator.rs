//! Opt-in desktop "Stop sharing" surface around the canonical host run.
//! One read-only child stays visible while this run is enabled, including idle
//! listening between peers. It must be mapped and responsive BEFORE the host
//! listener starts. It never approves a peer or alters admission, input, audio,
//! file, or clipboard policy. Losing it stops the original run and all shares.
//!
//! The caller thread checks native liveness independently of both the child-I/O
//! thread and the EXISTING host runtime. The scoped host coordinator creates no
//! second async runtime. On stop the original host is driven through its normal
//! teardown; its future, media children and firewall owner are never abandoned.
mod process;
pub use process::Error as IndicatorError;
use crate::{host_run::{self, Options, Reporter, StopHandle, policy}, input_process::ProcessLaunch};
use process::{Owner, Status};
use std::{fmt, path::Path, sync::Arc, thread, time::{Duration, Instant}};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Configuration,
    Entropy,
    Indicator(IndicatorError),
    Host(host_run::Error),
    HostPanicked,
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
            Self::HostPanicked => "host_panicked",
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

/// The locally installed `fr-observation-indicator` image; never a peer path.
/// Missing/unresponsive UI refuses rather than silently sharing without it.
/// Without this opt-in the caller continues to use `host_run::run_with_policy`.
pub fn run_with_policy(
    options: &Options,
    report: &Reporter,
    stop: &Arc<StopHandle>,
    policy: policy::Configuration,
    image: &Path,
) -> Result<(), Error> {
    if stop.is_requested() { return Ok(()); }
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|_| Error::Entropy)?;
    let launch = ProcessLaunch::new(image, &options.display, options.xauthority.as_deref(), u128::from_ne_bytes(bytes))
        .map_err(|_| Error::Configuration)?;
    let stopped = stop.clone();
    let mut owner = Owner::start(launch, move || stopped.request()).map_err(Error::Indicator)?;
    let result = drive(&owner, stop, || host_run::run_with_policy(options, report, stop, policy));
    // Keep the UI through host teardown. Joining the native-I/O worker proves
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
    serve: impl FnOnce() -> Result<(), host_run::Error> + Send,
) -> Result<(), Error> {
    let control = owner.control();
    loop {
        let status = control.status();
        if let Some(error) = fault(status) { return Err(Error::Indicator(error)); }
        if stop.is_requested() || matches!(status, Status::Stopped(_)) { return Ok(()); }
        if status == Status::Ready { break; }
        thread::sleep(Duration::from_millis(5));
    }
    thread::scope(|scope| {
        // Unwinding also asks the ORIGINAL host to stop before scope joins it.
        let _fence = Fence(stop);
        let host = scope.spawn(serve);
        let mut failure = None;
        loop {
            let status = control.status();
            if let Some(error) = fault(status) {
                failure.get_or_insert(error);
            }
            if matches!(status, Status::Stopped(_)) { stop.request(); }
            if host.is_finished() { break; }
            thread::sleep(Duration::from_millis(5));
        }
        // A failed host teardown is not hidden by a local click or native fault.
        host.join().map_err(|_| Error::HostPanicked)?.map_err(Error::Host)?;
        failure.map_or(Ok(()), |error| Err(Error::Indicator(error)))
    })
}
struct Fence<'a>(&'a StopHandle);
impl Drop for Fence<'_> {
    fn drop(&mut self) { self.0.request(); }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_failure_is_distinct_from_an_operator_stop_and_host_cleanup() {
        assert_eq!(fault(Status::Stopped(IndicatorError::LocalRevoke)), None);
        assert_eq!(fault(Status::Stopped(IndicatorError::Stopped)), None);
        assert_eq!(fault(Status::Stopped(IndicatorError::Expired)), Some(IndicatorError::Expired));
        assert_eq!(Error::Indicator(IndicatorError::Expired).code(), "observation_indicator_evidence_lost");
        assert_eq!(Error::Host(host_run::Error::Cleanup("source")).code(), "cleanup_incomplete");
    }
}
