//! Presentation reports are authority evidence, not advisory load telemetry.
//! The original stream supplies provenance; only the canonical session route
//! carries reports. No decoder, network callback or heartbeat grants control.
use super::{Error as MediaError, ObservationControl, host_now};
use asupersync::cx::Cx;
use fr_core::{limits::ProtocolLimits, time::HostInstant};
use fr_media::presented::{Decision, Reporter, Verifier};
use fr_transport::quic::{
    self, ConnectionBinding, Messages, Priority, QuicRecords, Route, StreamRoute,
};
use fr_wire::{
    Kind, PipelineState, Progress, SourceObservation, WireError,
    decoder::Binding,
    negotiation::{ControlBinding, Selection},
    presented as wire,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Binding,
    Wire(WireError),
    Proof(fr_media::presented::Error),
    Authority(MediaError),
    Transport(quic::Error),
    Closed,
}
#[derive(Clone, Copy)]
pub(crate) struct Setup {
    binding: Binding,
    limits: ProtocolLimits,
}
impl Setup {
    pub(crate) fn selected(
        selected: &Selection,
        parent: ControlBinding,
        mut view: Binding,
    ) -> Result<Option<Self>, Error> {
        let Some(capability) = selected
            .capabilities
            .iter()
            .find(|c| c.name == wire::CAPABILITY)
        else {
            return Ok(None);
        };
        if capability.version != wire::VERSION
            || parent.host_boot != view.parent.host_boot
            || parent.os_session != view.parent.os_session
            || parent.remote_session != view.parent.remote_session
        {
            return Err(Error::Binding);
        }
        if usize::try_from(selected.limits.max_control_message_bytes())
            .map_err(|_| Error::Binding)?
            < wire::BYTES
        {
            return Err(Error::Wire(WireError::ResourceLimit));
        }
        view.parent = parent;
        view.validate().map_err(Error::Wire)?;
        Ok(Some(Self {
            binding: view,
            limits: selected.limits,
        }))
    }
    fn route(self, q: &QuicRecords, route: StreamRoute, outbound: bool) -> Result<(), Error> {
        if route.binding != self.binding.parent.id
            || route.outbound != outbound
            || route.messages != Messages::SessionControl
            || route.priority != Priority::Critical
            || route.maximum < wire::BYTES
            || route.maximum > self.limits.max_control_message_bytes() as usize
            || !q.has_route(Route::Stream(route))
        {
            return Err(Error::Binding);
        }
        Ok(())
    }
}
pub(crate) fn is_report(bytes: &[u8]) -> bool {
    bytes.get(6..8) == Some(&(Kind::PresentedState as u16).to_be_bytes())
}
/// Private to the streaming owner. The verifier cannot be detached from its
/// original authority/connection or receive caller-selected source deadlines.
pub(crate) struct HostPresentation {
    connection: ConnectionBinding,
    inbound: Route,
    control: ObservationControl,
    verifier: Verifier,
    pub(crate) accepted: u64,
}
impl HostPresentation {
    pub(crate) fn attach(
        selected: &Selection,
        parent: ControlBinding,
        view: Binding,
        q: &QuicRecords,
        inbound: StreamRoute,
        control: ObservationControl,
    ) -> Result<Option<Self>, Error> {
        let Some(setup) = Setup::selected(selected, parent, view)? else {
            return Ok(None);
        };
        setup.route(q, inbound, false)?;
        if q.role() != Ok(asupersync::net::quic_native::StreamRole::Server)
            || q.is_closed()
            || !control.belongs_to_session(parent.remote_session)
        {
            return Err(Error::Binding);
        }
        control.check().map_err(Error::Authority)?;
        let verifier = {
            let mut authority = control
                .authority
                .lock()
                .map_err(|_| Error::Authority(MediaError::Poisoned))?;
            // Sample under the authority lock: independent native checks may
            // have overtaken a timestamp sampled before acquiring the mutex.
            let at = host_now(&control.cx).map_err(Error::Authority)?;
            let verifier =
                Verifier::new(setup.binding, setup.limits, at.as_micros()).map_err(Error::Proof)?;
            authority
                .require_view_evidence(at)
                .map_err(|e| Error::Authority(MediaError::Authority(e)))?;
            verifier
        };
        Ok(Some(Self {
            connection: q.binding(),
            inbound: Route::Stream(inbound),
            control,
            verifier,
            accepted: 0,
        }))
    }
    pub(crate) fn check_connection(&mut self, q: &QuicRecords) -> Result<(), Error> {
        if !q.is_bound_to(&self.connection) || q.is_closed() {
            self.stale();
            return Err(Error::Binding);
        }
        self.control.check().map_err(Error::Authority)?;
        Ok(())
    }
    /// The containing `StreamingHost` reads this ONLY from its admitted sender's
    /// packet cache, never from a remote report or native completion callback.
    pub(crate) fn observe(&mut self, progress: Option<Progress>) -> Result<(), Error> {
        let result = (|| {
            let at = self.control.check().map_err(Error::Authority)?.as_micros();
            let progress = progress.ok_or(Error::Closed)?;
            self.verifier.observe(progress, at).map_err(Error::Proof)?;
            if progress.observation == SourceObservation::Unknown
                || !matches!(
                    progress.pipeline,
                    PipelineState::Running | PipelineState::Idle
                )
            {
                self.stale();
            }
            Ok(())
        })();
        if result.is_err() {
            self.stale();
        }
        result
    }
    pub(crate) fn receive(&mut self, route: Route, bytes: &[u8]) -> Result<(), Error> {
        let result = (|| {
            if route != self.inbound {
                return Err(Error::Binding);
            }
            self.control.check().map_err(Error::Authority)?;
            let mut authority = self
                .control
                .authority
                .lock()
                .map_err(|_| Error::Authority(MediaError::Poisoned))?;
            let at = host_now(&self.control.cx).map_err(Error::Authority)?;
            match self
                .verifier
                .receive(bytes, at.as_micros())
                .map_err(Error::Proof)?
            {
                Decision::Ready { until_us } => {
                    authority
                        .mark_view_ready_until(HostInstant::from_micros(until_us), at)
                        .map_err(|e| Error::Authority(MediaError::Authority(e)))?;
                    self.accepted = self.accepted.saturating_add(1);
                }
                Decision::Unavailable => authority.mark_view_stale(),
                Decision::Obsolete => {}
            }
            Ok(())
        })();
        if result.is_err() {
            self.stale();
        }
        result
    }
    fn stale(&self) {
        if let Ok(mut authority) = self.control.authority.lock() {
            authority.mark_view_stale();
        }
    }
}
impl Drop for HostPresentation {
    fn drop(&mut self) {
        self.stale();
    }
}

/// A temporary compositor-to-visibility gap preserves the preceding host
/// deadline, but cannot send a positive report or extend readiness. Unknown or
/// stale source evidence sends one explicit negative report instead.
#[derive(Clone, Copy)]
pub(crate) enum ViewSample {
    Visible(wire::Sample, u64),
    Pending,
    Unavailable,
}
impl ViewSample {
    pub(crate) fn from_view(
        sample: Result<wire::Sample, fr_media::freshness::Error>,
        sampled_at: u64,
    ) -> Result<Self, fr_media::freshness::Error> {
        use fr_media::freshness::Error as E;
        match sample {
            Ok(s) if s.age_upper_us < wire::MAX_SOURCE_AGE_US => Ok(Self::Visible(s, sampled_at)),
            Ok(_) | Err(E::SourceUnknown | E::SourceStale) => Ok(Self::Unavailable),
            Err(E::NotSubmitted) => Ok(Self::Pending),
            Err(error) => Err(error),
        }
    }
}
/// One reporter survives the observation-to-control transition. It never owns
/// the decoder or creates a visibility sample from a native submission alone.
pub(crate) struct ViewerPresentation {
    connection: ConnectionBinding,
    outbound: Route,
    reporter: Reporter,
    pub(crate) sent: u64,
}
impl ViewerPresentation {
    pub(crate) fn attach(
        selected: &Selection,
        parent: ControlBinding,
        view: Binding,
        q: &QuicRecords,
        outbound: StreamRoute,
        now: u64,
    ) -> Result<Option<Self>, Error> {
        let Some(setup) = Setup::selected(selected, parent, view)? else {
            return Ok(None);
        };
        setup.route(q, outbound, true)?;
        if q.role() != Ok(asupersync::net::quic_native::StreamRole::Client) || q.is_closed() {
            return Err(Error::Binding);
        }
        Ok(Some(Self {
            connection: q.binding(),
            outbound: Route::Stream(outbound),
            reporter: Reporter::new(setup.binding, setup.limits, now).map_err(Error::Proof)?,
            sent: 0,
        }))
    }
    pub(crate) fn service(
        &mut self,
        q: &mut QuicRecords,
        cx: &Cx,
        sample: ViewSample,
        now: u64,
    ) -> Result<(), Error> {
        if !q.is_bound_to(&self.connection) || q.is_closed() {
            return Err(Error::Binding);
        }
        cx.checkpoint().map_err(|_| Error::Closed)?;
        match sample {
            ViewSample::Visible(sample, sampled_at) => {
                // Age was measured at sampled_at, not at this later service
                // call. Anchor the pending deadline to that original sample.
                if now < sampled_at {
                    return Err(Error::Proof(fr_media::presented::Error::Clock));
                }
                self.reporter.prepare(Some(sample), sampled_at)
            }
            ViewSample::Unavailable => self.reporter.prepare(None, now),
            ViewSample::Pending => self.reporter.pause(now),
        }
        .map_err(Error::Proof)?;
        if let Some((bytes, until)) = self.reporter.pending(now).map_err(Error::Proof)? {
            match q.send(cx, self.outbound, bytes, until, || {
                cx.checkpoint().is_ok()
                    && cx
                        .timer_driver()
                        .is_some_and(|t| t.now().as_nanos() / 1000 < until)
            }) {
                Ok(()) => {
                    self.reporter.queued(now).map_err(Error::Proof)?;
                    self.sent = self.sent.saturating_add(1);
                }
                Err(quic::Error::Backpressure) => {}
                Err(error) => return Err(Error::Transport(error)),
            }
        }
        Ok(())
    }
}
