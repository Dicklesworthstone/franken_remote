//! Desktop "Stop sharing" surface around the canonical host run.
//! Explicit on Linux; required by the macOS CLI for every host run; always
//! shown by a Windows host, which draws it itself (`windows`).
//! One read-only child stays visible while this run is enabled, including idle
//! listening between peers. It must be mapped and responsive BEFORE the host
//! listener starts. It never approves a peer or changes admission policy.
//!
//! The original host runtime stays on its caller's thread and original stack.
//! A scoped liveness watcher checks the native-I/O worker independently. UI
//! loss requests the original host's stop and normal teardown; neither the
//! host future nor its media/firewall cleanup is cancelled and abandoned.
pub mod microphone;
mod process;
#[cfg(all(host_stack, windows))]
mod windows;
#[cfg(host_stack)]
use crate::{
    host_run::{self, Encoder, Options, Reporter, StopHandle, policy},
    input_process::ProcessLaunch,
};
#[cfg(host_stack)]
pub use process::Error as IndicatorError;
#[cfg(all(host_stack, not(windows)))]
use process::Owner;
#[cfg(host_stack)]
use process::Status;
#[cfg(host_stack)]
use std::{
    fmt,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(host_stack)]
pub enum Error {
    Configuration,
    Entropy,
    Indicator(IndicatorError),
    Host(host_run::Error),
    WatchdogPanicked,
    Cleanup,
}
#[cfg(host_stack)]
impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Configuration => "observation_indicator_configuration",
            Self::Entropy => "entropy_unavailable",
            Self::Indicator(IndicatorError::Busy) => "observation_indicator_busy",
            Self::Indicator(IndicatorError::Expired) => "observation_indicator_evidence_lost",
            Self::Indicator(IndicatorError::Clock) => "observation_indicator_clock_failed",
            Self::Indicator(IndicatorError::Protocol) => "observation_indicator_protocol",
            Self::Indicator(_) => "observation_indicator_unavailable",
            Self::Host(error) => error.code(),
            Self::WatchdogPanicked => "observation_indicator_failed",
            Self::Cleanup => "cleanup_incomplete",
        }
    }
}
#[cfg(host_stack)]
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "frd-indicator: {self:?}")
    }
}
#[cfg(host_stack)]
impl std::error::Error for Error {}

/// Require the locally installed `fr-observation-indicator` before serving a
/// host. macOS also uses it for control-capable hosting; that path retains its
/// separate per-lease control indicator. Linux keeps its view-only selection.
/// Missing/unresponsive UI refuses rather than silently disabling the option.
/// This legacy entry point retains the explicit software encoder profile.
#[cfg(host_stack)]
pub fn run_with_policy(
    options: &Options,
    report: &Reporter,
    stop: &Arc<StopHandle>,
    policy: policy::Configuration,
    image: &Path,
) -> Result<(), Error> {
    run_with_policy_and_encoder(
        options,
        report,
        stop,
        policy,
        image,
        Encoder::SoftwareExplicit,
    )
}

/// Preserve the local encoder request through the indicator gate. Explicit
/// automatic hardware selection still requires real startup probes. The UI
/// never permits software fallback or changes the selected backend later.
#[cfg(host_stack)]
pub fn run_with_policy_and_encoder(
    options: &Options,
    report: &Reporter,
    stop: &Arc<StopHandle>,
    policy: policy::Configuration,
    image: &Path,
    encoder: impl Into<host_run::EncoderSelection>,
) -> Result<(), Error> {
    run_with_surface(options, stop, image, || {
        host_run::run_with_policy_and_encoder(options, report, stop, policy, encoder)
    })
}

/// A host with local approval still keeps its independent Stop Sharing
/// surface before listening, while prompting, and through final cleanup.
#[cfg(host_stack)]
pub fn run_with_desktop_approval(
    options: &Options,
    report: &Reporter,
    stop: &Arc<StopHandle>,
    policy: policy::Configuration,
    image: &Path,
    encoder: impl Into<host_run::EncoderSelection>,
    approval: &host_run::approval::Configuration,
) -> Result<(), Error> {
    run_with_surface(options, stop, image, || {
        host_run::run_with_desktop_approval(options, report, stop, policy, encoder, approval)
    })
}

/// On Windows `image` is unused: frd shows the window itself.
#[cfg(host_stack)]
fn run_with_surface(
    options: &Options,
    stop: &Arc<StopHandle>,
    image: &Path,
    serve: impl FnOnce() -> Result<(), host_run::Error>,
) -> Result<(), Error> {
    if stop.is_requested() {
        return Ok(());
    }
    if cfg!(target_os = "linux") && options.input_agent.is_some() {
        return Err(Error::Configuration);
    }
    #[cfg(windows)]
    {
        let _ = image;
        let stopped = stop.clone();
        let control = options.input_agent.is_some();
        let mut owner =
            windows::Owner::start(control, move || stopped.request()).map_err(Error::Indicator)?;
        let result = drive(&owner.control(), stop, serve);
        // Keep the window through host teardown, then close it.
        match owner.finish() {
            Ok(()) => result,
            Err(_) => Err(Error::Cleanup),
        }
    }
    #[cfg(not(windows))]
    run_with_process(options, stop, image, serve)
}

#[cfg(all(host_stack, not(windows)))]
fn run_with_process(
    options: &Options,
    stop: &Arc<StopHandle>,
    image: &Path,
    serve: impl FnOnce() -> Result<(), host_run::Error>,
) -> Result<(), Error> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|_| Error::Entropy)?;
    let launch = ProcessLaunch::new(
        image,
        &options.display,
        options.xauthority.as_deref(),
        u128::from_ne_bytes(bytes),
    )
    .map_err(|_| Error::Configuration)?;
    let stopped = stop.clone();
    let mut owner = Owner::start(launch, move || stopped.request()).map_err(Error::Indicator)?;
    let result = drive(&owner.control(), stop, serve);
    // Keep the UI through normal host teardown. Joining the I/O worker proves
    // its exact child was reaped; a timeout retains the original worker handle.
    owner.stop();
    let until = Instant::now() + Duration::from_secs(1);
    loop {
        match owner.try_finish() {
            Some(Ok(())) => return result,
            None if Instant::now() < until => thread::sleep(Duration::from_millis(5)),
            Some(Err(_)) | None => return Err(Error::Cleanup),
        }
    }
}
/// Once running, a local UI revocation is a normal stop. The macOS helper may
/// also receive a real local Stop before its first Ready reply. That cancels
/// startup successfully, so launchd's failure-only restart cannot undo the
/// user's choice. Other opening failures remain refusals.
#[cfg(host_stack)]
fn fault(status: Status) -> Option<IndicatorError> {
    match status {
        Status::Opening
        | Status::Ready
        | Status::Stopped(IndicatorError::LocalRevoke | IndicatorError::Stopped) => None,
        Status::Stopped(error) => Some(error),
    }
}
/// What a run's watcher reads from its Stop sharing surface.
#[cfg(host_stack)]
trait Watched: Sync {
    fn status(&self) -> Status;
    fn wait_for_stop(&self, finished: &AtomicBool) -> Option<IndicatorError>;
    fn wake_watcher(&self);
}
#[cfg(host_stack)]
impl Watched for process::Control {
    fn status(&self) -> Status {
        Self::status(self)
    }
    fn wait_for_stop(&self, finished: &AtomicBool) -> Option<IndicatorError> {
        Self::wait_for_stop(self, finished)
    }
    fn wake_watcher(&self) {
        Self::wake_watcher(self);
    }
}
#[cfg(all(host_stack, windows))]
impl Watched for windows::Control {
    fn status(&self) -> Status {
        Self::status(self)
    }
    fn wait_for_stop(&self, finished: &AtomicBool) -> Option<IndicatorError> {
        Self::wait_for_stop(self, finished)
    }
    fn wake_watcher(&self) {
        Self::wake_watcher(self);
    }
}
#[cfg(host_stack)]
fn drive(
    control: &impl Watched,
    stop: &StopHandle,
    serve: impl FnOnce() -> Result<(), host_run::Error>,
) -> Result<(), Error> {
    loop {
        let status = control.status();
        if let Status::Stopped(error) = status {
            if cfg!(target_os = "macos") && error == IndicatorError::LocalRevoke {
                return Ok(());
            }
            return Err(Error::Indicator(error));
        }
        if stop.is_requested() {
            // A worker may publish its terminal cause after the status sample
            // above but before this stop flag is observed. Read it again so a
            // failed opening cannot masquerade as a successful cancellation.
            if let Status::Stopped(error) = control.status()
                && !(cfg!(target_os = "macos") && error == IndicatorError::LocalRevoke)
            {
                return Err(Error::Indicator(error));
            }
            return Ok(());
        }
        if status == Status::Ready {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let done = AtomicBool::new(false);
    thread::scope(|scope| {
        // Unwind stops the ORIGINAL host and releases the watcher before the
        // scope joins it. No runtime coordinator is moved onto a smaller stack.
        let _finish = Finish {
            done: &done,
            stop,
            control,
        };
        let watcher = scope.spawn(|| {
            // A watcher panic also requests host stop before its thread exits.
            let _fence = Fence(stop);
            control
                .wait_for_stop(&done)
                .and_then(|error| fault(Status::Stopped(error)))
        });
        let result = serve().map_err(Error::Host);
        done.store(true, Ordering::Release);
        control.wake_watcher();
        let failure = watcher.join().map_err(|_| Error::WatchdogPanicked);
        // Preserve a failed host teardown even if native liveness also failed.
        result?;
        let failure = failure?.or_else(|| fault(control.status()));
        failure.map_or(Ok(()), |error| Err(Error::Indicator(error)))
    })
}
#[cfg(host_stack)]
struct Fence<'a>(&'a StopHandle);
#[cfg(host_stack)]
impl Drop for Fence<'_> {
    fn drop(&mut self) {
        self.0.request();
    }
}
#[cfg(host_stack)]
struct Finish<'a, W: Watched> {
    done: &'a AtomicBool,
    stop: &'a StopHandle,
    control: &'a W,
}
#[cfg(host_stack)]
impl<W: Watched> Drop for Finish<'_, W> {
    fn drop(&mut self) {
        self.stop.request();
        self.done.store(true, Ordering::Release);
        self.control.wake_watcher();
    }
}

#[cfg(all(test, host_stack))]
mod tests {
    use super::*;
    #[test]
    fn native_failure_is_distinct_from_local_revocation_and_host_cleanup() {
        assert_eq!(fault(Status::Stopped(IndicatorError::LocalRevoke)), None);
        assert_eq!(fault(Status::Stopped(IndicatorError::Stopped)), None);
        assert_eq!(
            fault(Status::Stopped(IndicatorError::Expired)),
            Some(IndicatorError::Expired)
        );
        assert_eq!(
            Error::Indicator(IndicatorError::Expired).code(),
            "observation_indicator_evidence_lost"
        );
        assert_eq!(
            Error::Host(host_run::Error::Cleanup("source")).code(),
            "cleanup_incomplete"
        );
    }
}
