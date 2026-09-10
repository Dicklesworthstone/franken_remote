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
