//! One bounded host-originated file attachment, serviced by the control owner.
//! No disk worker, file content or publication exists until the original
//! role-specific ticket exchange completes. Existing transport bounds own it.
use super::{Configuration, ControlledHost, Error, FilesChannel, HostReceiver, MediaChannel};
use asupersync::cx::Cx;
use fr_core::time::HostInstant;
use fr_files::session::Authority;
use fr_transport::quic::{
    self, ChannelRequest, ChannelScope, ConnectionBinding, ControlRoutes, QuicRecords, Route,
};
use fr_wire::{attachment, files, negotiation::Role};

pub(super) struct Pending {
    cx: Cx,
    connection: ConnectionBinding,
    control: ControlRoutes,
    channel: MediaChannel,
    authority: Authority,
    handle: u128,
    configuration: Configuration,
    last: u64,
}
impl Pending {
    pub(super) fn check(&mut self) -> Result<(), quic::Error> {
        self.cx.checkpoint().map_err(|_| quic::Error::Cancelled)?;
        let now = self
            .cx
            .timer_driver()
            .ok_or(quic::Error::Clock)?
            .now()
            .as_nanos()
            / 1000;
        if now < self.last {
            return Err(quic::Error::Clock);
        }
        if now >= self.channel.deadline_us() {
            return Err(quic::Error::Expired);
        }
        self.authority
            .deadline(HostInstant::from_micros(now))
            .map_err(|_| quic::Error::Unauthorized)?;
        if !self.configuration.permission.is_approved() {
            return Err(quic::Error::Unauthorized);
        }
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
    pub(super) fn advance(
        &mut self,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<bool, quic::Error> {
        // Verify opaque ownership BEFORE any call that can close/modify q. IDs
        // and route equality on a different connection do not confer authority.
        if !q.is_bound_to(&self.connection) {
            return Err(quic::Error::WrongRoute);
        }
        self.check()?;
        self.channel.transmit(q, &self.cx, &mut authorize)?;
        self.channel.dispatch(q, &self.cx, &mut authorize)?;
        let complete = self.channel.finish(q, &self.cx, &mut authorize)?.is_some();
        self.check()?;
        Ok(complete)
    }
    pub(super) fn start_receiver(self, q: &mut QuicRecords) -> Result<HostReceiver, Error> {
        if !q.is_bound_to(&self.connection) {
            return Err(Error::WrongBinding);
        }
        let lane = FilesChannel::new(q, self.channel, self.authority.binding().lease, self.handle)
            .map_err(Error::Transport)?;
        HostReceiver::spawn_with_authority(self.cx, q, lane, self.authority, self.configuration)
            .map_err(Error::Receiver)
    }
}
impl ControlledHost {
    /// Offer a separately bounded file lane on the existing controlled session.
    /// Normal `drive` turns perform the complete one-use role/ticket exchange,
    /// then start receipt on the locally selected directory and original input
    /// authority. No application callback must manually dispatch attachment data.
    ///
    /// `request` uses a fresh binding and unpredictable host ticket for the
    /// approved current view. `handle` is the nonzero file-scope handle already
    /// agreed with the peer, NOT a path or a replacement for local permission.
    /// This method does not publish that handle or grant remote filesystem access.
    /// Initial permission denial allocates no transport/disk owner. Revocation
    /// during an unfinished setup fences the parent rather than discarding its
    /// final unacknowledged handshake. Completed transfers retire independently.
    pub fn offer_files(
        &mut self,
        request: ChannelRequest,
        handle: u128,
        configuration: Configuration,
    ) -> Result<(), Error> {
        if self.files.used {
            return Err(Error::AlreadyAttached);
        }
        self.session.check().map_err(|_| Error::Closed)?;
        let selected = &self.session.opened.selected;
        if selected.role != Role::RequestControl
            || ![
                (attachment::CAPABILITY, attachment::VERSION),
                (attachment::INPUT_CAPABILITY, attachment::INPUT_VERSION),
                (attachment::FILES_CAPABILITY, attachment::FILES_VERSION),
                (files::CAPABILITY, files::VERSION),
            ]
            .iter()
            .all(|(name, version)| {
                selected
                    .capabilities
                    .iter()
                    .any(|c| c.name == *name && c.version == *version)
            })
        {
            return Err(Error::NotNegotiated);
        }
        if handle == 0 {
            return Err(Error::WrongBinding);
        }
        if !configuration.permission.is_approved() {
            return Err(Error::PermissionRequired);
        }
        let cx = &self.session.opened.cx;
        let last = cx.timer_driver().ok_or(Error::Clock)?.now().as_nanos() / 1000;
        let q = &mut self.session.opened.transport;
        let authority = self.input.file_authority(q).map_err(|_| Error::Closed)?;
        let observation = &self.session.opened.control;
        let channel = q
            .offer_media_role(
                cx,
                ChannelScope {
                    control: self.session.opened.routes,
                    parent: self.session.opened.binding,
                    selection: selected,
                },
                request,
                attachment::MediaRole::Files,
                || {
                    observation.check().is_ok()
                        && crate::session_startup::now(cx).is_ok_and(|now| {
                            authority.deadline(HostInstant::from_micros(now)).is_ok()
                        })
                },
            )
            .map_err(Error::Transport)?;
        // Retain all ownership before returning to the caller. Native startup
        // and its fallible allocations occur only after authenticated completion.
        self.files.pending = Some(Pending {
            cx: cx.clone(),
            connection: q.binding(),
            control: self.session.opened.routes,
            channel,
            authority,
            handle,
            configuration,
            last,
        });
        self.files.used = true;
        Ok(())
    }
}
