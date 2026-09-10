//! Join one completed input attachment to the existing native owner. Neither
//! an attached stream nor a control-intent capability grants an input lease.
use super::{Error, QuicInput, Routes};
use crate::{input_agent::Agent, media::ObservationControl};
use asupersync::cx::Cx;
use fr_core::{input::InputView, limits::ProtocolLimits};
use fr_transport::quic::{
    AttachedChannel, DatagramRoute, Disposition, MediaChannel, Messages, QuicRecords, Route,
    StreamRoute,
};
use fr_wire::{
    attachment::{self, MediaRole},
    control::Request,
    decoder::Binding,
    input::{InputDelivery, InputDirection, decode_input},
    negotiation::{ControlBinding, Role, Selection},
};

/// Consumes the non-cloneable input attachment. A copied route descriptor is
/// not enough to construct this join, and a second native owner cannot consume
/// the same proof. The configuration owner stays with its existing media task.
/// The enclosing session still owns consent, view invalidation and cleanup.
pub struct NegotiatedInput {
    input: MediaChannel,
    configuration: AttachedChannel,
    parent: ControlBinding,
    limits: ProtocolLimits,
}
impl std::fmt::Debug for NegotiatedInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NegotiatedInput([connection-bound input routes])")
    }
}
fn view(binding: Binding) -> InputView {
    InputView {
        geometry: binding.geometry,
        viewport: binding.viewport,
        configuration: binding.configuration,
        recovery: binding.recovery,
    }
}
impl NegotiatedInput {
    pub fn new(
        q: &QuicRecords,
        selection: &Selection,
        configuration: &MediaChannel,
        input: MediaChannel,
    ) -> Result<Self, Error> {
        selection.validate().map_err(|_| Error::InvalidRoutes)?;
        if selection.role != Role::RequestControl
            || !selection.capabilities.iter().any(|c| {
                c.name == attachment::INPUT_CAPABILITY && c.version == attachment::INPUT_VERSION
            })
            || configuration
                .completed_limits(q)
                .map_err(Error::Transport)?
                != selection.limits
            || input.completed_limits(q).map_err(Error::Transport)? != selection.limits
        {
            return Err(Error::InvalidRoutes);
        }
        let parent = input.completed_parent(q).map_err(Error::Transport)?;
        if configuration
            .completed_parent(q)
            .map_err(Error::Transport)?
            != parent
        {
            return Err(Error::InvalidRoutes);
        }
        let configuration = configuration.completed_on(q).map_err(Error::Transport)?;
        let attached = input.completed_on(q).map_err(Error::Transport)?;
        let a = configuration.descriptor.binding;
        let mut b = attached.descriptor.binding;
        if a.parent.id == b.parent.id {
            return Err(Error::InvalidRoutes);
        }
        b.parent.id = a.parent.id;
        if configuration.descriptor.role != MediaRole::Configuration
            || attached.descriptor.role != MediaRole::Input
            || a != b
        {
            return Err(Error::InvalidRoutes);
        }
        let owner = Self {
            input,
            configuration,
            parent,
            limits: selection.limits,
        };
        owner.checked(q)?;
        Ok(owner)
    }
    fn checked(&self, q: &QuicRecords) -> Result<AttachedChannel, Error> {
        let a = self.input.completed_on(q).map_err(Error::Transport)?;
        for pair in [a, self.configuration] {
            if !q.has_route(Route::Stream(pair.outbound))
                || !q.has_route(Route::Stream(pair.inbound))
                || q.receive_ended(pair.inbound).map_err(Error::Transport)?
            {
                return Err(Error::Closed);
            }
        }
        let pointer = a.datagram.ok_or(Error::InvalidRoutes)?;
        if pointer.kind != 0x0042 || !q.has_route(Route::Datagram(pointer)) {
            return Err(Error::InvalidRoutes);
        }
        Ok(a)
    }
    pub const fn limits(&self) -> ProtocolLimits {
        self.limits
    }
    pub fn channel_binding(&self) -> u32 {
        self.input.descriptor().binding.parent.id
    }
    /// Check the full control parent, displayed channel and view before the
    /// separate local control-grant operation. Bounds and capabilities must
    /// still match that operation's locally selected, probed target.
    pub fn check_request(&self, q: &QuicRecords, request: Request) -> Result<(), Error> {
        self.checked(q)?;
        if !self.matches_request(request) {
            return Err(Error::InvalidRoutes);
        }
        Ok(())
    }
    pub(super) fn matches_request(&self, request: Request) -> bool {
        request.parent == self.parent
            && request.target.display_binding == self.configuration.descriptor.binding.parent.id
            && request.target.view == view(self.input.descriptor().binding)
    }
    /// Validate the retained proof before the broker consumes its one-time
    /// observation attachment. Equal numeric routes are not a substitute.
    pub(super) fn broker_routes(
        &self,
        q: &QuicRecords,
        parent: ControlBinding,
        limits: ProtocolLimits,
    ) -> Result<Routes, Error> {
        if parent != self.parent || limits != self.limits {
            return Err(Error::InvalidRoutes);
        }
        let attached = self.checked(q)?;
        Routes::new(attached.inbound, attached.outbound, attached.datagram)
    }
    /// Attach an already granted native agent, not a factory supplied by a peer.
    /// It must own the exact same observation authority and mapped view. This
    /// operation consumes the attachment proof but does not consume the native
    /// control-renewal owner, which remains separately attachable once.
    pub fn into_host(
        self,
        cx: Cx,
        agent: Agent,
        q: &QuicRecords,
        observation: &ObservationControl,
    ) -> Result<QuicInput, Error> {
        let a = self.checked(q)?;
        let pointer = a.datagram.ok_or(Error::InvalidRoutes)?;
        let binding = self.input.descriptor().binding;
        if pointer.outbound
            || agent.protocol_limits() != self.limits
            || !agent.matches_observation_view(
                observation,
                self.parent.remote_session,
                view(binding),
            )
            || observation.check_control().is_err()
        {
            return Err(Error::InvalidRoutes);
        }
        let routes = Routes::new(a.inbound, a.outbound, Some(pointer))?;
        QuicInput::new(cx, agent, q, routes)
    }
    /// Viewer routes stay checked against their original connection. The caller
    /// must still use `InputClient`'s lease, ticket, mapping and presented-state
    /// gates to construct bytes; this function does not enable input itself.
    pub fn viewer_routes(
        &self,
        q: &QuicRecords,
    ) -> Result<(StreamRoute, StreamRoute, DatagramRoute), Error> {
        let a = self.checked(q)?;
        let pointer = a.datagram.ok_or(Error::InvalidRoutes)?;
        if !pointer.outbound
            || a.outbound.messages != Messages::InputActions
            || a.inbound.messages != Messages::InputFeedback
        {
            return Err(Error::InvalidRoutes);
        }
        Ok((a.outbound, a.inbound, pointer))
    }
    /// Preserve the caller's original absolute deadline and exact record under
    /// transport backpressure. An action never falls back to a pointer datagram.
    pub fn send(
        &self,
        cx: &Cx,
        q: &mut QuicRecords,
        bytes: &[u8],
        deadline_us: u64,
        authorize: impl FnMut() -> bool,
    ) -> Result<(), Error> {
        let (actions, _, pointer) = self.viewer_routes(q)?;
        let delivery = if bytes.get(6..8) == Some(&0x0042_u16.to_be_bytes()) {
            InputDelivery::Datagram
        } else {
            InputDelivery::Reliable
        };
        if bytes.get(6..8) == Some(&(fr_wire::Kind::HeldState as u16).to_be_bytes()) {
            let request = fr_wire::held_state::decode(
                bytes,
                &self.limits,
                self.channel_binding(),
                InputDirection::ViewerToHost,
                delivery,
            )
            .map_err(Error::Wire)?;
            if request.session != self.parent.remote_session {
                return Err(Error::InvalidRoutes);
            }
        } else {
            let record = decode_input(
                bytes,
                &self.limits,
                self.channel_binding(),
                InputDirection::ViewerToHost,
                delivery,
            )
            .map_err(Error::Wire)?;
            if record.credentials.session != self.parent.remote_session
                || record.credentials.view != view(self.input.descriptor().binding)
            {
                return Err(Error::InvalidRoutes);
            }
        }
        let route = if delivery == InputDelivery::Datagram {
            Route::Datagram(pointer)
        } else {
            Route::Stream(actions)
        };
        q.send(cx, route, bytes, deadline_us, authorize)
            .map_err(Error::Transport)
    }
    /// Only this viewer's exact result/ticket stream is dispatched. Other
    /// session and media records remain owned by their existing drivers.
    pub fn receive_ready(
        &self,
        cx: &Cx,
        q: &mut QuicRecords,
        authorize: impl FnMut() -> bool,
        mut deliver: impl FnMut(&[u8]) -> Result<Disposition, ()>,
    ) -> Result<usize, Error> {
        let (_, feedback, _) = self.viewer_routes(q)?;
        q.receive_ready(
            cx,
            authorize,
            |r| r == Route::Stream(feedback),
            |_, bytes| deliver(bytes),
        )
        .map_err(Error::Transport)
    }
}
