//! Opt-in composition for the selected user's interactive input process.
//! Never load Xlib in the broker/media worker. No listener, approval bypass,
//! implicit display selection, Wayland fallback, or new authority is provided.
use crate::input::{X11Pointer, local_display};
use asupersync::cx::Cx;
use fr_core::{
    input::InputBounds,
    input_submission::{Capabilities, InputSession, PlatformError},
};
use frd::input_agent::{Agent, Driver, Error as AgentError, Route, Seat};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidDisplay,
    Agent(AgentError),
}
/// Start only after local identity/approval, view readiness and controller
/// selection. The supplied session must use `frd::input_watchdog::host_now(cx)`.
/// Poll the returned driver in the Asupersync authority region. The sink opens
/// only on its native thread, and that thread owns all release/restoration.
/// Factory revalidation refuses changed geometry or unsupported negotiated
/// capabilities before any input can be submitted.
pub fn start_x11(
    seat: &Seat,
    cx: Cx,
    session: InputSession,
    route: Route,
    display: &str,
) -> Result<(Agent, Driver), Error> {
    let factory = x11_factory(display, session.bounds(), session.capabilities())?;
    seat.start(cx, session, route, factory, X11Pointer::cleanup_native)
        .map_err(Error::Agent)
}
/// Prepare the same qualified factory for broker-managed initial grants. Merely
/// building this closure opens no display and submits no input. Execute it only
/// on the canonical native thread; it revalidates exact bounds and capabilities.
/// Pass `X11Pointer::cleanup_native` as the matching native cleanup function.
pub fn x11_factory(
    display: &str,
    expected_bounds: InputBounds,
    required: Capabilities,
) -> Result<impl FnOnce() -> Result<X11Pointer, PlatformError> + Send + 'static, Error> {
    if !local_display(display) {
        return Err(Error::InvalidDisplay);
    }
    let display = display.to_owned();
    Ok(move || {
        let sink = X11Pointer::open(&display)?;
        if sink.bounds() != expected_bounds {
            return Err(PlatformError::GeometryChanged);
        }
        if !sink.capabilities().contains_all(required) {
            return Err(PlatformError::Unsupported);
        }
        Ok(sink)
    })
}

/// Start the original X11 input owner with a final logind checkpoint on every
/// native operation. The local display and effective UID must match the explicit
/// watch selection. Watch remains independently owned until native reap.
/// Poll `GuardedDriver` in the original authority region: it checks evidence on
/// the existing watchdog cadence even while no remote input arrives. No new
/// worker, timer queue, runtime or grant is introduced by this composition.
#[cfg(feature = "linux-logind")]
pub fn start_x11_guarded(
    seat: &Seat,
    cx: Cx,
    session: InputSession,
    route: Route,
    display: &str,
    evidence: crate::logind::Control,
) -> Result<(Agent, GuardedDriver), Error> {
    if !evidence.matches_local_x11(display) {
        return Err(Error::InvalidDisplay);
    }
    let factory = x11_factory(display, session.bounds(), session.capabilities())?;
    let monitor = session.monitor();
    let native_evidence = evidence.clone();
    let (agent, driver) = seat
        .start(
            cx,
            session,
            route,
            move || {
                if native_evidence.status() != crate::logind::Status::Active {
                    return Err(PlatformError::Permission);
                }
                let sink = factory()?;
                if native_evidence.status() != crate::logind::Status::Active {
                    return Err(PlatformError::Permission);
                }
                Ok(crate::logind::input::Gate::new(
                    sink,
                    native_evidence,
                    monitor,
                ))
            },
            // Canonical core cleanup revokes first and releases its own held state.
            // Native cleanup additionally restores X11 state (e.g. repeat settings).
            |gate| X11Pointer::cleanup_native(&mut gate.inner),
        )
        .map_err(Error::Agent)?;
    Ok((
        agent,
        GuardedDriver {
            inner: driver,
            evidence,
        },
    ))
}

/// The original input driver plus negative local-session evidence. Its existing
/// bounded watchdog timer services evidence expiry independently of input traffic.
/// The returned Shutdown still reports actual native/core cleanup, not merely
/// revocation. Dropping even an unpolled driver retains canonical abandonment.
#[cfg(feature = "linux-logind")]
#[must_use = "poll in the authority region and retain the native shutdown receipt"]
pub struct GuardedDriver {
    inner: Driver,
    evidence: crate::logind::Control,
}
#[cfg(feature = "linux-logind")]
impl std::future::Future for GuardedDriver {
    type Output = frd::input_agent::Shutdown;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        task: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.get_mut();
        this.evidence.register(task);
        let status = this.evidence.status();
        if status != crate::logind::Status::Active {
            let reason = if matches!(
                status,
                crate::logind::Status::Stopped(
                    crate::logind::StopReason::Locked | crate::logind::StopReason::Suspending
                )
            ) {
                frd::input_watchdog::StopReason::Suspended
            } else {
                frd::input_watchdog::StopReason::AuthorityEnded
            };
            this.inner.control().stop(reason);
        }
        std::pin::Pin::new(&mut this.inner).poll(task)
    }
}
