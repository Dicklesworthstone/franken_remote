//! A single role-8 exchange on the original post-grant connection.
use crate::{input_quic::QuicInput, microphone_process as native, microphone_quic as uplink};
use asupersync::cx::Cx;
use fr_core::time::HostInstant;
use fr_media::audio::authority::HostController;
use fr_transport::quic::{
    ChannelRequest, ChannelScope, ConnectionBinding, ControlRoutes, MediaChannel, QuicRecords,
    Route, audio::AudioUpChannel,
};
use fr_wire::{attachment::MediaRole, negotiation::Selection};

pub(super) struct Pending {
    cx: Cx,
    connection: ConnectionBinding,
    control: ControlRoutes,
    channel: MediaChannel,
    controller: HostController,
    selection: Selection,
    launch: native::Launch,
    last: u64,
}
fn at(cx: &Cx) -> Result<u64, uplink::Error> {
    cx.checkpoint().map_err(|_| uplink::Error::Cancelled)?;
    Ok(cx.timer_driver().ok_or(uplink::Error::Clock)?.now().as_nanos() / 1000)
}
impl Pending {
    pub(super) fn check(&mut self) -> Result<(), uplink::Error> {
        let now = at(&self.cx)?;
        if now < self.last {
            return Err(uplink::Error::Clock);
        }
        if now >= self.channel.deadline_us() {
            return Err(uplink::Error::SetupExpired);
        }
        self.controller
            .deadline(HostInstant::from_micros(now))
            .map_err(uplink::Error::Authority)?;
        self.last = now;
        Ok(())
    }
    pub(super) fn owns(&self, route: Route, bytes: &[u8]) -> bool {
        (route == Route::Stream(self.control.inbound)
            && bytes.get(6..8).is_some_and(|kind| {
                (0x0018..=0x001c).contains(&u16::from_be_bytes([kind[0], kind[1]]))
            }))
            || matches!(route, Route::Stream(r) if r.binding == self.channel.descriptor().binding.parent.id)
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn offer(
        cx: &Cx,
        q: &mut QuicRecords,
        scope: ChannelScope<'_>,
        request: ChannelRequest,
        controller: HostController,
        launch: native::Launch,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<Self, uplink::Error> {
        if !fr_wire::audio_up::uplink_selected(scope.selection) {
            return Err(uplink::Error::NotSelected);
        }
        let last = at(cx)?;
        controller
            .deadline(HostInstant::from_micros(last))
            .map_err(uplink::Error::Authority)?;
        if controller.scope().session != scope.parent.remote_session {
            return Err(uplink::Error::Binding);
        }
        let channel = q
            .offer_media_role(cx, scope, request, MediaRole::AudioUp, || {
                authorize()
                    && at(cx).is_ok_and(|t| {
                        controller.deadline(HostInstant::from_micros(t)).is_ok()
                    })
            })
            .map_err(uplink::Error::Transport)?;
        Ok(Self {
            cx: cx.clone(),
            connection: q.binding(),
            control: scope.control,
            channel,
            controller,
            selection: scope.selection.clone(),
            launch,
            last,
        })
    }
    pub(super) fn advance(
        &mut self,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<bool, uplink::Error> {
        if !q.is_bound_to(&self.connection) {
            return Err(uplink::Error::WrongConnection);
        }
        self.check()?;
        let controller = &self.controller;
        let cx = &self.cx;
        let until = self.channel.deadline_us();
        let mut permitted = || {
            authorize()
                && at(cx).is_ok_and(|t| {
                    t < until && controller.deadline(HostInstant::from_micros(t)).is_ok()
                })
        };
        self.channel
            .transmit(q, cx, &mut permitted)
            .map_err(uplink::Error::Transport)?;
        self.channel
            .dispatch(q, cx, &mut permitted)
            .map_err(uplink::Error::Transport)?;
        let complete = self
            .channel
            .finish(q, cx, &mut permitted)
            .map_err(uplink::Error::Transport)?
            .is_some();
        self.check()?;
        Ok(complete)
    }
    pub(super) fn join(
        mut self,
        q: &mut QuicRecords,
        input: &QuicInput,
    ) -> Result<uplink::Host, uplink::Error> {
        if !q.is_bound_to(&self.connection) {
            return Err(uplink::Error::WrongConnection);
        }
        self.check()?;
        let lane = AudioUpChannel::new(q, self.channel).map_err(uplink::Error::Transport)?;
        uplink::Host::new(q, lane, input, &self.selection, true, self.launch)
            .map_err(|error| error.reason())
    }
}
