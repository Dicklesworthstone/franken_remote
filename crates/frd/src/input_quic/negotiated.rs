//! Join one completed input attachment to the existing native owner. Neither
//! an attached stream nor a control-intent capability grants an input lease.
mod host_recovery;
use super::{Error, QuicInput, Routes, ticket_bytes};
use crate::{input_agent::Agent, media::ObservationControl, media_quic::NegotiatedMedia};
use asupersync::cx::Cx;
use fr_core::{ids::RecoveryGeneration, input::InputView, limits::ProtocolLimits};
use fr_transport::quic::{
    AttachedChannel, DatagramRoute, Disposition, MediaChannel, Messages, QuicRecords, Route,
    SendAdmission, StreamRoute,
};
use fr_wire::{
    attachment::{self, MediaRole},
    control::Request,
    decoder::Binding,
    input::{InputDelivery, InputDirection, decode_input},
    input_ticket::{self, Profile},
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
    ticket_profile: Profile,
    reference_recovery: bool,
    /// The reference-recovery generation input records must name: the original
    /// attachment's, advanced only by [`NegotiatedInput::follow_recovery`].
    recovery: RecoveryGeneration,
    /// A reference recovery is replacing the media channels: the configuration
    /// channel may end before its successor is followed. The grant's input is
    /// suspended for the whole recovery (plan 11.3).
    recovering: bool,
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
/// `new` is the configuration a reference recovery installs after `old`: a new
/// channel whose binding is otherwise identical, one recovery generation on.
fn recovered_successor(old: Binding, new: Binding) -> bool {
    let Some(next) = old.recovery.next() else {
        return false;
    };
    let mut expected = old;
    expected.recovery = next;
    expected.parent.id = new.parent.id;
    new.parent.id != 0 && new.parent.id != old.parent.id && expected == new
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
        let ticket_profile = input_ticket::selected_profile(selection).map_err(Error::Wire)?;
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
        if ticket_profile != Profile::Legacy
            && ![attached.inbound, attached.outbound].iter().any(|route| {
                route.messages == Messages::InputFeedback
                    && route.maximum >= ticket_bytes(ticket_profile)
            })
        {
            return Err(Error::InvalidRoutes);
        }
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
            ticket_profile,
            reference_recovery: selection.capabilities.iter().any(|cap| {
                cap.name == fr_wire::recovery_request::CAPABILITY
                    && cap.version == fr_wire::recovery_request::VERSION
            }),
            recovery: a.recovery,
            recovering: false,
        };
        owner.checked(q)?;
        Ok(owner)
    }
    fn checked(&self, q: &QuicRecords) -> Result<AttachedChannel, Error> {
        let a = self.input.completed_on(q).map_err(Error::Transport)?;
        let pairs = [a, self.configuration];
        let checked = if self.recovering {
            &pairs[..1]
        } else {
            &pairs[..]
        };
        for &pair in checked {
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
    /// Whether the original negotiated profile includes the actual authority cap.
    pub const fn authority_tickets(&self) -> bool {
        matches!(self.ticket_profile, Profile::Authority | Profile::Readiness)
    }
    /// The exact profile selected on this original input attachment. Decoders
    /// never infer it from a record's length or downgrade it after admission.
    pub const fn ticket_profile(&self) -> Profile {
        self.ticket_profile
    }
    /// The view input records must name: the attachment's, at the current
    /// recovery generation.
    fn current_view(&self) -> InputView {
        let mut binding = self.input.descriptor().binding;
        binding.recovery = self.recovery;
        view(binding)
    }
    /// Begin the media-lifetime gap on the original viewer connection, after
    /// negotiated reference loss. The caller suspends input authority first.
    /// A failed check changes nothing; repetition cannot renew any deadline.
    pub fn begin_recovery(&mut self, q: &QuicRecords) -> Result<(), Error> {
        if !self.reference_recovery {
            return Err(Error::InvalidRoutes);
        }
        let previous = self.recovering;
        self.recovering = true;
        if let Err(error) = self.viewer_routes(q) {
            self.recovering = previous;
            return Err(error);
        }
        Ok(())
    }
    /// Validate the completed replacement without advancing the input view.
    /// Use this before advancing a presentation owner. Success is neither
    /// presentation evidence nor permission to send input.
    pub fn check_recovery(&self, q: &QuicRecords, media: &NegotiatedMedia) -> Result<(), Error> {
        self.recovery_configuration(q, media).map(|_| ())
    }
    fn recovery_configuration(
        &self,
        q: &QuicRecords,
        media: &NegotiatedMedia,
    ) -> Result<AttachedChannel, Error> {
        if !self.reference_recovery || !self.recovering || *media.limits().protocol() != self.limits
        {
            return Err(Error::InvalidRoutes);
        }
        // Both non-forgeable owners must belong to THIS connection. A copied
        // AttachedChannel with matching numeric routes is not such a proof.
        self.viewer_routes(q)?;
        media.check(q).map_err(|_| Error::InvalidRoutes)?;
        // This also checks viewer direction and positive recovery negotiation.
        // The control role is retained by this original input attachment.
        media
            .retiring_viewer_routes(q)
            .map_err(|_| Error::InvalidRoutes)?;
        let configuration = media.configuration_channel();
        if configuration.descriptor.binding.parent.id == self.channel_binding()
            || configuration.descriptor.role != MediaRole::Configuration
            || !recovered_successor(
                self.configuration.descriptor.binding,
                configuration.descriptor.binding,
            )
        {
            return Err(Error::InvalidRoutes);
        }
        Ok(configuration)
    }
    /// Follow the exact successor from a live completed media owner. The
    /// original input attachment, lease and sequence domain are unchanged.
    /// Failed validation leaves the recovery fence and old binding intact.
    pub fn follow_recovery(
        &mut self,
        q: &QuicRecords,
        media: &NegotiatedMedia,
    ) -> Result<(), Error> {
        let configuration = self.recovery_configuration(q, media)?;
        let previous = (self.configuration, self.recovery, self.recovering);
        self.configuration = configuration;
        self.recovery = configuration.descriptor.binding.recovery;
        self.recovering = false;
        if let Err(error) = self.checked(q) {
            (self.configuration, self.recovery, self.recovering) = previous;
            return Err(error);
        }
        Ok(())
    }
    pub fn channel_binding(&self) -> u32 {
        self.input.descriptor().binding.parent.id
    }
    /// Check the full control parent, displayed channel and view before the
    /// separate local control-grant operation. Bounds and capabilities must
    /// still match that operation's locally selected, probed target.
    pub fn check_request(&self, q: &QuicRecords, request: Request) -> Result<(), Error> {
        self.checked(q)?;
        if self.recovering || !self.matches_request(request) {
            return Err(Error::InvalidRoutes);
        }
        Ok(())
    }
    pub(super) fn matches_request(&self, request: Request) -> bool {
        request.parent == self.parent
            && request.target.display_binding == self.configuration.descriptor.binding.parent.id
            && request.target.view == self.current_view()
    }
    /// Validate the retained proof before the broker consumes its one-time
    /// observation attachment. Equal numeric routes are not a substitute.
    pub(super) fn broker_routes(
        &self,
        q: &QuicRecords,
        parent: ControlBinding,
        limits: ProtocolLimits,
    ) -> Result<Routes, Error> {
        if self.recovering || parent != self.parent || limits != self.limits {
            return Err(Error::InvalidRoutes);
        }
        let attached = self.checked(q)?;
        Routes::new(attached.inbound, attached.outbound, attached.datagram)
    }
    /// Exact host-side configuration identity for joining a capture pipeline.
    /// This validates the original attachment and host route direction, not
    /// consent, a usable view, or permission to initialize a native input owner.
    pub(crate) fn host_scope(&self, q: &QuicRecords) -> Result<(ControlBinding, Binding), Error> {
        self.broker_routes(q, self.parent, self.limits)?;
        Ok((self.parent, self.configuration.descriptor.binding))
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
        if self.recovering
            || pointer.outbound
            || agent.protocol_limits() != self.limits
            || !agent.matches_observation_view(
                observation,
                self.parent.remote_session,
                self.current_view(),
            )
            || observation.check_control().is_err()
        {
            return Err(Error::InvalidRoutes);
        }
        let routes = Routes::new(a.inbound, a.outbound, Some(pointer))?;
        QuicInput::with_ticket_profile(cx, agent, q, routes, self.ticket_profile)
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
    /// Current checked configuration identity; unavailable during replacement.
    pub fn viewer_scope(&self, q: &QuicRecords) -> Result<(ControlBinding, Binding), Error> {
        self.viewer_routes(q)?;
        if self.recovering {
            return Err(Error::InvalidRoutes);
        }
        Ok((self.parent, self.configuration.descriptor.binding))
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
        match self.send_prepared(cx, q, bytes, deadline_us, authorize, || {
            Ok::<(), core::convert::Infallible>(())
        })? {
            SendAdmission::Accepted => Ok(()),
            SendAdmission::Refused(never) => match never {},
        }
    }
    /// Recheck a prepared input record independently of connection authority.
    /// A temporary presentation gap admits no input bytes, but cannot close the
    /// connection that carries lease maintenance and the visibility evidence.
    /// The original record/deadline and all negotiated identity checks remain
    /// mandatory. Cancellation and terminal connection loss take precedence.
    pub fn send_prepared<E>(
        &self,
        cx: &Cx,
        q: &mut QuicRecords,
        bytes: &[u8],
        deadline_us: u64,
        authorize: impl FnMut() -> bool,
        prepared: impl FnMut() -> Result<(), E>,
    ) -> Result<SendAdmission<E>, Error> {
        let (actions, _, pointer) = self.viewer_routes(q)?;
        if self.recovering {
            // The parent continues renewal/result service, never old input.
            // This also fences pre-encoded actions and held-state records.
            return Err(Error::InvalidRoutes);
        }
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
                || record.credentials.view != self.current_view()
            {
                return Err(Error::InvalidRoutes);
            }
        }
        let route = if delivery == InputDelivery::Datagram {
            Route::Datagram(pointer)
        } else {
            Route::Stream(actions)
        };
        q.send_prepared(cx, route, bytes, deadline_us, authorize, prepared)
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

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, HostBootId, OsSessionId,
        RemoteSessionId, ViewportMappingGeneration,
    };

    fn binding(id: u32, recovery: RecoveryGeneration) -> Binding {
        Binding {
            parent: ControlBinding {
                id,
                host_boot: HostBootId::from_raw(1),
                os_session: OsSessionId::from_raw(2),
                remote_session: RemoteSessionId::from_raw(3),
            },
            display: 4,
            geometry: DisplayGeometryGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery,
            viewport: ViewportMappingGeneration::INITIAL,
        }
    }

    #[test]
    fn only_the_next_recovery_generation_on_a_new_channel_is_followed() {
        let first = RecoveryGeneration::INITIAL;
        let next = first.next().unwrap();
        let old = binding(10, first);
        assert!(recovered_successor(old, binding(20, next)));
        // The same channel, the same generation, or a skipped generation is not
        // a recovery of this configuration.
        assert!(!recovered_successor(old, binding(10, next)));
        assert!(!recovered_successor(old, binding(20, first)));
        assert!(!recovered_successor(old, binding(20, next.next().unwrap())));
        // Every other authority/display/codec field must be unchanged.
        let mut other = binding(20, next);
        other.parent.remote_session = RemoteSessionId::from_raw(9);
        assert!(!recovered_successor(old, other));
        let mut other = binding(20, next);
        other.display = 5;
        assert!(!recovered_successor(old, other));
        let mut other = binding(20, next);
        other.configuration = other.configuration.next().unwrap();
        assert!(!recovered_successor(old, other));
    }
}

#[cfg(test)]
mod recovery_tests;
