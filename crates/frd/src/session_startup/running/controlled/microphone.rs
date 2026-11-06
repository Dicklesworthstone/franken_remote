//! Optional microphone retained by the original running controller.
use super::ControlledHost;
use crate::{microphone_process as native, microphone_quic as uplink};
use fr_transport::quic::{ChannelRequest, ChannelScope, QuicRecords, Route, audio::AudioUpChannel};

mod negotiation;
use negotiation::Pending;

#[derive(Default)]
pub(super) struct Slot {
    used: bool,
    pending: Option<Pending>,
    host: Option<uplink::Host>,
    reason: Option<uplink::Error>,
}
impl Slot {
    pub(super) fn raw_io_available(&self) -> bool {
        self.pending.is_none()
            && self
                .host
                .as_ref()
                .is_none_or(|h| h.state() == uplink::State::Retired)
    }
    pub(super) fn stop(&mut self) {
        self.used = true;
        if let Some(host) = &self.host {
            host.stop();
        }
        if self.pending.take().is_some() {
            self.reason.get_or_insert(uplink::Error::Cancelled);
        }
    }
    pub(super) fn permitted(&mut self) -> bool {
        if let Some(pending) = &mut self.pending
            && let Err(error) = pending.check()
        {
            self.reason.get_or_insert(error);
            self.pending = None;
            return false;
        }
        self.host.as_ref().is_none_or(uplink::Host::permits_io)
    }
    pub(super) fn owns(&self, route: Route, bytes: &[u8]) -> bool {
        self.pending.as_ref().is_some_and(|p| p.owns(route, bytes))
            || self.host.as_ref().is_some_and(|h| h.owns_inbound(route))
    }
    pub(super) fn maintain(&mut self, q: &mut QuicRecords) -> Result<(), uplink::Error> {
        if let Some(pending) = &mut self.pending {
            pending.check().inspect_err(|error| {
                self.reason.get_or_insert(*error);
            })?;
        }
        if let Some(host) = &mut self.host {
            host.maintain(q).inspect_err(|error| {
                self.reason.get_or_insert(*error);
            })?;
        }
        Ok(())
    }
    pub(super) fn service(
        &mut self,
        q: &mut QuicRecords,
        input: &crate::input_quic::QuicInput,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<(), uplink::Error> {
        if let Some(pending) = &mut self.pending
            && pending.advance(q, &mut authorize).inspect_err(|error| {
                self.reason.get_or_insert(*error);
            })?
        {
            let pending = self.pending.take().ok_or(uplink::Error::Closed)?;
            self.host = Some(pending.join(q, input).inspect_err(|error| {
                self.reason.get_or_insert(*error);
            })?);
        }
        if let Some(host) = &mut self.host {
            host.service(q, authorize).inspect_err(|error| {
                self.reason.get_or_insert(*error);
            })?;
        }
        Ok(())
    }
}

impl ControlledHost {
    /// Offer one separately approved microphone pair on the original controller.
    /// Ordinary drive turns complete the fixed-deadline role/ticket exchange.
    /// No native process starts before the viewer explicitly requests talk.
    /// Missing selection or local approval refuses before allocating a route.
    pub fn offer_microphone(
        &mut self,
        request: ChannelRequest,
        approved: bool,
        launch: native::Launch,
    ) -> Result<(), uplink::Error> {
        self.offer_microphone_with_permission(request, approved, launch, None)
    }
    /// A separately approved native application may additionally require its
    /// original local privacy surface to stay live throughout negotiation and
    /// native output. This check only restricts the existing controller grant.
    pub(crate) fn offer_microphone_with_permission(
        &mut self,
        request: ChannelRequest,
        approved: bool,
        launch: native::Launch,
        permission: Option<std::sync::Arc<dyn Fn() -> bool + Send + Sync>>,
    ) -> Result<(), uplink::Error> {
        if self.microphone.used {
            return Err(uplink::Error::AlreadyAttached);
        }
        self.session.check().map_err(|_| uplink::Error::Closed)?;
        if !approved {
            return Err(uplink::Error::Permission);
        }
        let input = &self.input;
        let opened = &mut self.session.opened;
        let controller = input
            .microphone_controller(&opened.transport)
            .map_err(uplink::Error::Input)?;
        let pending = Pending::offer(
            &opened.cx,
            &mut opened.transport,
            ChannelScope {
                control: opened.routes,
                parent: opened.binding,
                selection: &opened.selected,
            },
            request,
            controller,
            launch,
            permission,
            || opened.control.check().is_ok(),
        )?;
        self.microphone.pending = Some(pending);
        self.microphone.used = true;
        Ok(())
    }

    pub fn microphone_negotiating(&self) -> bool {
        self.microphone.pending.is_some()
    }

    /// Join one completed `AudioUp` pair after the original input grant.
    /// Approval is separate local microphone consent; worker paths are local
    /// installation choices. Failure returns the original non-cloneable lane.
    /// A foreign or second pair is never dropped, retired or adopted on error.
    pub fn attach_microphone(
        &mut self,
        lane: AudioUpChannel,
        approved: bool,
        launch: native::Launch,
    ) -> Result<uplink::Control, uplink::AttachError> {
        if let Err(error) = lane.check(&self.session.opened.transport) {
            return Err(uplink::AttachError::new(
                uplink::Error::Transport(error),
                lane,
            ));
        }
        if self.microphone.used {
            return Err(uplink::AttachError::new(
                uplink::Error::AlreadyAttached,
                lane,
            ));
        }
        if self.session.check().is_err() {
            return Err(uplink::AttachError::new(uplink::Error::Closed, lane));
        }
        let host = uplink::Host::new(
            &mut self.session.opened.transport,
            lane,
            &self.input,
            &self.session.opened.selected,
            approved,
            launch,
        )?;
        let control = host.control();
        self.microphone.host = Some(host);
        self.microphone.used = true;
        Ok(control)
    }

    pub fn microphone_control(&self) -> Option<uplink::Control> {
        self.microphone.host.as_ref().map(uplink::Host::control)
    }

    /// Local state only; Running does not prove application selection or sound.
    pub fn microphone_state(&self) -> Option<uplink::State> {
        self.microphone.host.as_ref().map(uplink::Host::state)
    }

    pub fn microphone_reason(&self) -> Option<uplink::Error> {
        self.microphone
            .host
            .as_ref()
            .and_then(uplink::Host::reason)
            .or(self.microphone.reason)
    }

    pub fn microphone_statistics(&self) -> Option<uplink::Statistics> {
        self.microphone.host.as_ref().map(uplink::Host::statistics)
    }

    /// Actual device AND codec exit receipts, separate from requested stop.
    pub fn microphone_retirement(&self) -> Option<native::Retirement> {
        self.microphone
            .host
            .as_ref()
            .and_then(uplink::Host::retirement)
    }

    /// Fence native output first and retire the original optional pair.
    /// Drained retirement preserves control. An unfinished handshake or
    /// uncertain retained native output conservatively closes its parent.
    pub fn retire_microphone(&mut self) -> Result<(), uplink::Error> {
        if self.microphone.pending.is_some() {
            self.close();
            return Err(uplink::Error::Cancelled);
        }
        let result = self.microphone.host.as_mut().map_or(Ok(()), |host| {
            host.retire(&mut self.session.opened.transport)
        });
        if result.is_err() {
            // An unsafe retained native send has already fenced transport.
            // End the original input owner in this same public operation.
            self.close();
        }
        result
    }
}
