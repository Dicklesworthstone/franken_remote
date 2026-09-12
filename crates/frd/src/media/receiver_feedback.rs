//! Exact-session advisory feedback on the canonical solicited metrics protocol.
//! No decoder or authority lock; one bounded pending query/reply per owner.
use asupersync::cx::Cx;
use fr_core::limits::ProtocolLimits;
use fr_media::{
    delivery::ReceivePipeline,
    pacing::ReceiverEvidence,
    receiver_feedback::{Requester, Responder},
};
use fr_transport::quic::{self, QuicRecords, Route};
use fr_wire::{
    Kind, WireError,
    decoder::Binding,
    negotiation::{ControlBinding, Selection},
    receiver_metrics::{self as wire, Load},
};

const WORK_RETENTION_US: u64 = 250_000;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Binding,
    Wire(WireError),
    Load(fr_media::receiver_feedback::Error),
    Transport(quic::Error),
    Closed,
    Clock,
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
        let Some(cap) = selected
            .capabilities
            .iter()
            .find(|cap| cap.name == wire::CAPABILITY)
        else {
            return Ok(None);
        };
        if cap.version != wire::VERSION
            || parent.host_boot != view.parent.host_boot
            || parent.os_session != view.parent.os_session
            || parent.remote_session != view.parent.remote_session
        {
            return Err(Error::Binding);
        }
        if usize::try_from(selected.limits.max_control_message_bytes())
            .map_err(|_| Error::Binding)?
            < wire::REPLY_BYTES
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
}
pub(crate) fn is_feedback(bytes: &[u8]) -> bool {
    bytes.get(6..8) == Some(&(Kind::StageMetrics as u16).to_be_bytes())
}
pub(crate) struct HostFeedback {
    inbound: Route,
    outbound: Route,
    requester: Requester,
    pub(crate) accepted: u64,
}
impl HostFeedback {
    pub(crate) fn new(setup: Setup, inbound: Route, outbound: Route) -> Result<Self, Error> {
        Ok(Self {
            inbound,
            outbound,
            requester: Requester::new(setup.binding, setup.limits, 0).map_err(Error::Load)?,
            accepted: 0,
        })
    }
    pub(crate) fn receive(&mut self, route: Route, bytes: &[u8], now: u64) -> Result<(), Error> {
        if route != self.inbound {
            return Err(Error::Binding);
        }
        if self.requester.receive(bytes, now).map_err(Error::Load)? {
            self.accepted = self.accepted.saturating_add(1);
        }
        Ok(())
    }
    pub(crate) fn evidence(&mut self, now: u64) -> Result<ReceiverEvidence, Error> {
        self.requester.evidence(now).map_err(Error::Load)
    }
    pub(crate) fn service(&mut self, q: &mut QuicRecords, cx: &Cx, now: u64) -> Result<(), Error> {
        self.requester.prepare(now).map_err(Error::Load)?;
        if let Some((bytes, until)) = self.requester.pending(now).map_err(Error::Load)? {
            match q.send(cx, self.outbound, bytes, until, || before(cx, until)) {
                Ok(()) => self.requester.queued(now).map_err(Error::Load)?,
                Err(quic::Error::Backpressure) => {}
                Err(e) => return Err(Error::Transport(e)),
            }
        }
        Ok(())
    }
}
pub(crate) struct ViewerFeedback {
    inbound: Route,
    outbound: Route,
    responder: Responder,
    started: Option<u64>,
    completed: Option<(u64, u64)>,
    last_now: Option<u64>,
    pub(crate) sent: u64,
}
impl ViewerFeedback {
    pub(crate) fn new(setup: Setup, inbound: Route, outbound: Route) -> Result<Self, Error> {
        Ok(Self {
            inbound,
            outbound,
            responder: Responder::new(setup.binding, setup.limits, 0).map_err(Error::Load)?,
            started: None,
            completed: None,
            last_now: None,
            sent: 0,
        })
    }
    fn clock(&mut self, now: u64) -> Result<(), Error> {
        if self.last_now.is_some_and(|old| now < old) {
            return Err(Error::Clock);
        }
        self.last_now = Some(now);
        Ok(())
    }
    pub(crate) fn begin(&mut self, now: u64) -> Result<(), Error> {
        self.clock(now)?;
        if self.started.is_some() {
            return Err(Error::Closed);
        }
        self.started = Some(now);
        Ok(())
    }
    pub(crate) fn complete(&mut self, now: u64) -> Result<(), Error> {
        self.clock(now)?;
        let started = self.started.take().ok_or(Error::Closed)?;
        self.completed = Some((now, now.checked_sub(started).ok_or(Error::Clock)?));
        Ok(())
    }
    fn sample(&mut self, receiver: &ReceivePipeline, now: u64) -> Result<Load, Error> {
        self.clock(now)?;
        let usage = receiver.budget_usage();
        let active = self.started.map(|start| now - start);
        let recent = self
            .completed
            .filter(|&(at, _)| now - at < WORK_RETENTION_US)
            .map(|(_, us)| us);
        Ok(Load {
            retained_bytes: u64::try_from(usage.bytes).map_err(|_| Error::Binding)?,
            retained_pictures: u32::try_from(usage.pictures).map_err(|_| Error::Binding)?,
            decoding: self.started.is_some(),
            work_us: active.into_iter().chain(recent).max(),
        })
    }
    pub(crate) fn receive(
        &mut self,
        route: Route,
        bytes: &[u8],
        receiver: &ReceivePipeline,
        now: u64,
    ) -> Result<(), Error> {
        if route != self.inbound {
            return Err(Error::Binding);
        }
        let load = self.sample(receiver, now)?;
        self.responder
            .receive(bytes, load, now)
            .map_err(Error::Load)?;
        Ok(())
    }
    /// Canonical renewal, repair and input work is serviced before telemetry.
    pub(crate) fn service(&mut self, q: &mut QuicRecords, cx: &Cx, now: u64) -> Result<(), Error> {
        self.clock(now)?;
        if let Some((bytes, until)) = self.responder.pending(now).map_err(Error::Load)? {
            match q.send(cx, self.outbound, bytes, until, || before(cx, until)) {
                Ok(()) => {
                    self.responder.queued(now).map_err(Error::Load)?;
                    self.sent = self.sent.saturating_add(1);
                }
                Err(quic::Error::Backpressure) => {}
                Err(e) => return Err(Error::Transport(e)),
            }
        }
        Ok(())
    }
}
fn before(cx: &Cx, until: u64) -> bool {
    cx.checkpoint().is_ok()
        && cx
            .timer_driver()
            .is_some_and(|t| t.now().as_nanos() / 1000 < until)
}
#[cfg(test)]
pub(crate) mod tests;
