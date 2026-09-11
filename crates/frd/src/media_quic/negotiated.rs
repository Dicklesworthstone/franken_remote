//! Join completed native attachments to the actual media owners. This is not
//! admission: the containing session still selects the view and services consent,
//! observation renewal, revocation and input cleanup independently of the codec.
use super::{Error, QuicEgress, Routes};
use crate::{
    media::{ObservationControl, Subscription, decoder_startup::Setup},
    media_egress::Egress,
};
use fr_media::delivery::{MediaBindings, MediaEpoch, ReceiveConfig, ReceivePolicy, SendPolicy};
use fr_transport::quic::{
    AttachedChannel, ConnectionBinding, Disposition, MediaChannel, QuicRecords, Route, StreamRoute,
};
use fr_wire::{
    Channel, MediaLimits, attachment::MediaRole, decoder::Binding, negotiation::Selection,
};
use std::time::Duration;

/// Immutable media wiring derived from three completed, one-use attachments.
/// No numeric route list or copied attachment descriptor can construct this.
/// Every use checks the original connection and all retained stream lifetimes.
/// Dropping it does not revoke a shared capture worker or another viewer.
pub struct NegotiatedMedia {
    connection: ConnectionBinding,
    configuration: AttachedChannel,
    recovery: AttachedChannel,
    video: AttachedChannel,
    selection: Selection,
    bindings: MediaBindings,
    limits: MediaLimits,
}
impl std::fmt::Debug for NegotiatedMedia {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NegotiatedMedia([connection-bound media routes])")
    }
}
fn same_view(a: Binding, mut b: Binding) -> bool {
    // Auxiliary binding IDs differ; every authority/display/codec generation
    // must agree. This comparison does not ignore any parent identity component.
    b.parent.id = a.parent.id;
    a == b
}
impl NegotiatedMedia {
    pub fn new(
        q: &QuicRecords,
        selection: &Selection,
        configuration: &MediaChannel,
        recovery: &MediaChannel,
        video: &MediaChannel,
    ) -> Result<Self, Error> {
        selection.validate().map_err(|_| Error::InvalidRoutes)?;
        for channel in [configuration, recovery, video] {
            if channel.completed_limits(q).map_err(Error::Transport)? != selection.limits {
                return Err(Error::InvalidRoutes);
            }
        }
        let (configuration, recovery, video) = (
            configuration.completed_on(q).map_err(Error::Transport)?,
            recovery.completed_on(q).map_err(Error::Transport)?,
            video.completed_on(q).map_err(Error::Transport)?,
        );
        let binding = configuration.descriptor.binding;
        if configuration.descriptor.role != MediaRole::Configuration
            || recovery.descriptor.role != MediaRole::Recovery
            || video.descriptor.role != MediaRole::Video
            || !same_view(binding, recovery.descriptor.binding)
            || !same_view(binding, video.descriptor.binding)
            || binding.parent.id == recovery.descriptor.binding.parent.id
            || binding.parent.id == video.descriptor.binding.parent.id
        {
            return Err(Error::InvalidRoutes);
        }
        let bindings = MediaBindings::negotiated(
            video.descriptor.binding.parent.id,
            recovery.descriptor.binding.parent.id,
        )
        .map_err(|_| Error::InvalidRoutes)?;
        // The single packetizer record ceiling must fit every actual media
        // lane, including the negotiated datagram and feedback allowance.
        let maximum = video.byte_allowance.min(recovery.byte_allowance);
        let maximum = usize::try_from(maximum)
            .map_err(|_| Error::InvalidRoutes)?
            .min(video.outbound.maximum)
            .min(video.inbound.maximum)
            .min(recovery.outbound.maximum)
            .min(recovery.inbound.maximum);
        let limits = MediaLimits::new(
            selection.limits,
            maximum,
            fr_wire::MAX_FRAGMENTS,
            fr_wire::MAX_REPAIR_RANGES,
        )
        .map_err(|_| Error::InvalidRoutes)?;
        let this = Self {
            connection: q.binding(),
            configuration,
            recovery,
            video,
            selection: selection.clone(),
            bindings,
            limits,
        };
        this.check(q)?;
        // Also enforce the existing decoder capability and exact route contract.
        this.decoder_setup(q, Duration::from_secs(2))
            .map_err(|_| Error::InvalidRoutes)?;
        Ok(this)
    }
    pub fn check(&self, q: &QuicRecords) -> Result<(), Error> {
        if !q.is_bound_to(&self.connection) {
            return Err(Error::ForeignConnection);
        }
        if q.is_closed() {
            return Err(Error::Closed);
        }
        for pair in [self.configuration, self.recovery, self.video] {
            if !q.has_route(Route::Stream(pair.outbound))
                || !q.has_route(Route::Stream(pair.inbound))
            {
                return Err(Error::InvalidRoutes);
            }
            if q.receive_ended(pair.inbound).map_err(Error::Transport)? {
                return Err(Error::Closed);
            }
        }
        if !self
            .video
            .datagram
            .is_some_and(|r| q.has_route(Route::Datagram(r)))
        {
            return Err(Error::InvalidRoutes);
        }
        Ok(())
    }
    pub const fn limits(&self) -> MediaLimits {
        self.limits
    }
    pub const fn binding(&self) -> Binding {
        self.configuration.descriptor.binding
    }
    pub const fn bindings(&self) -> MediaBindings {
        self.bindings
    }
    fn epoch(&self) -> MediaEpoch {
        let b = self.binding();
        MediaEpoch {
            configuration: b.configuration,
            recovery: b.recovery,
        }
    }
    pub fn decoder_setup(
        &self,
        q: &QuicRecords,
        timeout: Duration,
    ) -> Result<Setup, crate::media::decoder_startup::Error> {
        self.check(q)
            .map_err(|_| crate::media::decoder_startup::Error::InvalidRoutes)?;
        let (configuration, replies) = if self.is_host() {
            (self.configuration.outbound, self.configuration.inbound)
        } else {
            (self.configuration.inbound, self.configuration.outbound)
        };
        Setup::new(
            self.binding(),
            configuration,
            replies,
            &self.selection,
            timeout,
        )
    }
    fn is_host(&self) -> bool {
        self.video.datagram.is_some_and(|r| r.outbound)
    }
    /// The per-viewer sender inherits exactly the admitted binding and byte caps.
    /// The supplied control must be this session's already-approved observation.
    pub fn sender(
        &self,
        q: &QuicRecords,
        control: ObservationControl,
        policy: SendPolicy,
    ) -> Result<QuicEgress, Error> {
        self.check(q)?;
        if !self.is_host() || !control.belongs_to_session(self.binding().parent.remote_session) {
            return Err(Error::InvalidRoutes);
        }
        let routes = Routes::new(
            self.bindings,
            self.video.outbound,
            self.recovery.outbound,
            self.video.datagram.ok_or(Error::InvalidRoutes)?,
            self.video.inbound,
        )?;
        let subscription =
            Subscription::new(control, self.limits, self.bindings, self.epoch(), policy)
                .map_err(Error::Media)?;
        Ok(QuicEgress {
            egress: Egress::new(subscription),
            routes,
            connection: Some(self.connection.clone()),
            view: Some(self.binding()),
        })
    }
    pub fn receiver_config(
        &self,
        q: &QuicRecords,
        policy: ReceivePolicy,
    ) -> Result<ReceiveConfig, Error> {
        self.check(q)?;
        if self.is_host() {
            return Err(Error::InvalidRoutes);
        }
        Ok(ReceiveConfig {
            limits: self.limits,
            bindings: self.bindings,
            epoch: self.epoch(),
            policy,
        })
    }
    /// The exact reliable source-progress route for a viewer dispatcher.
    pub fn progress_route(&self, q: &QuicRecords) -> Result<StreamRoute, Error> {
        self.check(q)?;
        if self.is_host() {
            return Err(Error::InvalidRoutes);
        }
        Ok(self.video.inbound)
    }
    #[cfg(test)]
    pub(crate) fn progress_for_test(&self, q: &QuicRecords) -> StreamRoute {
        self.check(q).unwrap();
        assert!(self.is_host());
        self.video.outbound
    }
    /// Called from this connection's bounded dispatcher. Kind and stream remain
    /// exact even though progress, repair and datagrams share the Video binding.
    pub fn viewer_channel(&self, q: &QuicRecords, route: Route) -> Result<Channel, Error> {
        self.check(q)?;
        self.received_channel(route)
    }
    fn received_channel(&self, route: Route) -> Result<Channel, Error> {
        if self.is_host() {
            return Err(Error::InvalidRoutes);
        }
        if route == Route::Stream(self.recovery.inbound) {
            return Ok(Channel::Recovery);
        }
        if route == Route::Stream(self.video.inbound) {
            return Ok(Channel::MediaConfig);
        }
        if self
            .video
            .datagram
            .is_some_and(|r| route == Route::Datagram(r))
        {
            return Ok(Channel::Video);
        }
        Err(Error::InvalidRoutes)
    }
    /// Copy the authenticated dispatch map for a split-borrow session turn.
    /// Callers must revalidate this owner on that connection before each turn.
    pub(crate) fn viewer_routes(&self, q: &QuicRecords) -> Result<[(Route, Channel); 3], Error> {
        self.check(q)?;
        if self.is_host() {
            return Err(Error::InvalidRoutes);
        }
        Ok([
            (Route::Stream(self.recovery.inbound), Channel::Recovery),
            (Route::Stream(self.video.inbound), Channel::MediaConfig),
            (
                Route::Datagram(self.video.datagram.ok_or(Error::InvalidRoutes)?),
                Channel::Video,
            ),
        ])
    }
    /// Dispatch only the negotiated viewer media lanes from their original live
    /// connection. No borrowed packet escapes this bounded transport callback;
    /// all unrelated records stay with their existing session owner.
    pub fn receive_ready(
        &self,
        cx: &asupersync::cx::Cx,
        q: &mut QuicRecords,
        authorize: impl FnMut() -> bool,
        mut deliver: impl FnMut(Channel, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<(), Error> {
        self.check(q)?;
        if self.is_host() {
            return Err(Error::InvalidRoutes);
        }
        q.receive_ready(
            cx,
            authorize,
            |r| self.received_channel(r).is_ok(),
            |r, b| deliver(self.received_channel(r).map_err(|_| ())?, b),
        )
        .map_err(Error::Transport)?;
        self.check(q)
    }
    /// On the host this is inbound; on the viewer it is outbound.
    pub fn repair_stream(&self, q: &QuicRecords) -> Result<StreamRoute, Error> {
        self.check(q)?;
        Ok(if self.is_host() {
            self.video.inbound
        } else {
            self.video.outbound
        })
    }
}
