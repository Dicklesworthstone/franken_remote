//! Transfer source ownership out of its first peer's guarded native callback.
use super::super::{LocalAction, Report};
use super::{Error, NativeDesktop, SessionAgent};
use crate::media::ObservationControl;
use crate::session_startup::shared_viewers::{Admission, Entropy, HostService};
use crate::worker::{self, Deadline};
use asupersync::cx::Cx;
use std::{future::Future, task::Context, time::Duration};

impl NativeDesktop {
    /// Continuous source/local-event service MUST belong to the OS-share owner,
    /// not to the first native connection callback. That connection can leave
    /// while siblings remain. No local permission is synthesized by this method.
    pub fn serve<'a, L>(
        &'a mut self,
        agent: &'a mut SessionAgent,
        capture_interval: Duration,
        entropy: Entropy,
        local: L,
    ) -> Result<impl Future<Output = Result<Report, Error>> + Send + use<'a, L>, Error>
    where
        L: FnMut(&mut SessionAgent, &mut Context<'_>) -> Result<LocalAction, ()> + Send + 'a,
    {
        agent.serve_shared_desktop(
            &mut self.prepared.publisher,
            &mut self.hub,
            capture_interval,
            entropy,
            local,
        )
    }
    /// Fence at call time, then observe the exact child under a separate cleanup
    /// context. Timeout/drop retains this desktop and never starts a replacement.
    pub fn reap<'a>(
        &'a mut self,
        cleanup: &'a Cx,
        deadline: Deadline,
    ) -> impl Future<Output = Result<asupersync::process::ExitStatus, worker::Error>> + 'a {
        self.close();
        self.prepared.publisher.reap(cleanup, deadline)
    }
    /// Transfer the original local agent and desktop to their independent
    /// OS-share service, and retain ONLY the first viewer's connection scope.
    /// Await the returned service inside the native host callback. Returning
    /// immediately after publication instead would cancel that first connection.
    ///
    /// `publish` is a one-use, bounded/nonblocking LOCAL ownership handoff, such
    /// as storing the pair in an empty single-slot application channel. The
    /// receiver must drive `desktop.serve(&mut agent, ...)` independently and
    /// retain the desktop through child reap. No worker/task, source clone,
    /// replacement connection, fresh deadline or retry is created here.
    ///
    /// Refusal, panic or reentrant source revocation fences the offered source
    /// and its whole cohort, even if the callback has already stored them or
    /// queued a sibling viewer. After SUCCESS, dropping the returned connection
    /// service fences only the first viewer, never the independent shared source.
    pub fn handoff(
        self,
        mut agent: SessionAgent,
        publish: impl FnOnce(SessionAgent, Self) -> Result<(), ()>,
    ) -> Result<HostService, Error> {
        let source = self
            .hub
            .service_owner(&self.prepared.publisher)
            .map_err(Error::Viewers)?;
        let registration = agent
            .register_original_source(&self.prepared.publisher)
            .map_err(Error::Consent)?;
        registration.recheck(&agent).map_err(Error::Consent)?;
        let service = self.first().into_service().map_err(Error::Viewers)?;
        let mut fence = Publication {
            source,
            admission: self.admissions(),
            transferred: false,
        };
        publish(agent, self).map_err(|()| Error::LocalEvent)?;
        fence
            .source
            .check()
            .map_err(|e| Error::Capture(crate::media::shared_publisher::Error::Media(e)))?;
        fence.admission.statistics().map_err(Error::Viewers)?;
        fence.transferred = true;
        Ok(service)
    }
}
struct Publication {
    source: ObservationControl,
    admission: Admission,
    transferred: bool,
}
impl Drop for Publication {
    fn drop(&mut self) {
        if !self.transferred {
            self.admission.fence();
            self.source.revoke();
        }
    }
}
