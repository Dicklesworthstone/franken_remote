//! Session-owned native capture. The platform owns its native thread and the
//! original controlled viewer owns its lifetime. No alternate input/authority path.
use super::{ControlledViewer, Layout, Source};
use crate::{session_startup::ControlledViewerError, worker::Deadline};
use asupersync::cx::Cx;
use std::future::Future;

/// One native producer's nonblocking lifecycle boundary. `stop` must not wait on
/// native calls, locks, the network, or a thread join. `try_reap` may join ONLY an
/// already finished thread and reports cleanup, not host-held-state release.
/// Native failure must stop the supplied Source, including while the viewer is
/// not being polled. Implementations must also stop on drop.
pub trait NativeCapture: Send + Sync {
    fn stop(&self);
    fn try_reap(&mut self) -> bool;
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureCleanup {
    NotStarted,
    /// The native owner still exists; it may be running or draining after stop.
    Pending,
    /// Native cleanup finished. This does not acknowledge host input release.
    Complete,
}
/// Failure to observe native cleanup, not a claim that input remains authorized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureReapError {
    Cancelled,
    Clock,
    Expired,
}
impl std::fmt::Display for CaptureReapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for CaptureReapError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureStartError<E> {
    Viewer(ControlledViewerError),
    Native(E),
}
impl ControlledViewer {
    /// Start exactly one native producer on the ORIGINAL granted viewer, after
    /// the renderer has acknowledged this exact placement. Merely configuring a
    /// layout is insufficient. No mapping or visible-frame evidence is invented.
    ///
    /// The factory must only start bounded native work (never perform blocking
    /// platform setup here). It receives the original one-use event Source. Its
    /// handle stays owned across streaming handoff, close, errors and abandonment
    /// of unpolled service futures. A failed or panicking factory closes the
    /// session rather than leaving a partly attached producer with live authority.
    pub fn capture_input_owned<C: NativeCapture + 'static, E>(
        &mut self,
        layout: &Layout,
        start: impl FnOnce(Source) -> Result<C, E>,
    ) -> Result<(), CaptureStartError<E>> {
        // Validate before invoking any platform code or consuming the Source.
        self.check().map_err(CaptureStartError::Viewer)?;
        self.viewport
            .check_layout(layout)
            .map_err(|e| CaptureStartError::Viewer(ControlledViewerError::Viewport(e)))?;
        if self.events.is_some() || self.native_capture.is_some() {
            return Err(CaptureStartError::Viewer(ControlledViewerError::Capture(
                super::Error::AlreadyAttached,
            )));
        }
        let mut operation = super::super::Operation {
            viewer: self,
            complete: false,
        };
        let source = operation
            .viewer
            .capture_input()
            .map_err(CaptureStartError::Viewer)?;
        let native = start(source).map_err(CaptureStartError::Native)?;
        operation.viewer.native_capture = Some(Box::new(native));
        // Recheck the original deadlines after the factory returns. Native setup
        // is not permission to extend the lease, view or queued-event lifetime.
        operation
            .viewer
            .check()
            .map_err(CaptureStartError::Viewer)?;
        operation.complete = true;
        Ok(())
    }
    /// Fence the original session at CALL time and wait for its native producer
    /// to finish under an independent cleanup context and the supplied absolute
    /// deadline. Expiry, cancellation and abandonment RETAIN the same producer;
    /// the caller may collect it later but must not start a replacement yet.
    /// Complete proves only native cleanup, never remote key/button release.
    pub fn reap_input_capture<'a>(
        &'a mut self,
        cleanup: &'a Cx,
        deadline: Deadline,
    ) -> impl Future<Output = Result<CaptureCleanup, CaptureReapError>> + 'a {
        self.close();
        wait_for_capture(cleanup, deadline, || self.input_capture_cleanup())
    }
    /// Nonblocking cleanup observation, also available after closure. `close`
    /// requests stop; a Pending result retains the owner for subsequent reaping.
    pub fn input_capture_cleanup(&mut self) -> CaptureCleanup {
        match &mut self.native_capture {
            None => CaptureCleanup::NotStarted,
            Some(native) => {
                if native.try_reap() {
                    CaptureCleanup::Complete
                } else {
                    CaptureCleanup::Pending
                }
            }
        }
    }
}

/// Shared bounded wait for the original owner; no blocking native join and no
/// reset timeout. An expired/cancelled wait does not consume a completion claim.
pub(crate) async fn wait_for_capture(
    cx: &Cx,
    deadline: Deadline,
    mut collect: impl FnMut() -> CaptureCleanup,
) -> Result<CaptureCleanup, CaptureReapError> {
    let mut previous = cx.timer_driver().ok_or(CaptureReapError::Clock)?.now();
    loop {
        cx.checkpoint().map_err(|_| CaptureReapError::Cancelled)?;
        let now = cx.timer_driver().ok_or(CaptureReapError::Clock)?.now();
        if now < previous {
            return Err(CaptureReapError::Clock);
        }
        previous = now;
        if now >= deadline.time() {
            return Err(CaptureReapError::Expired);
        }
        let state = collect();
        if state != CaptureCleanup::Pending {
            return Ok(state);
        }
        let wake = now.saturating_add_nanos(1_000_000).min(deadline.time());
        asupersync::time::sleep_until(wake).await;
    }
}
