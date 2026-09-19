//! One expected file-role attachment on the original viewer connection.
//! Configuration is bounded metadata; no source thread exists before `send_file`.
use super::{ControlledViewer, Error, Permission, Policy};
use asupersync::cx::Cx;
use fr_core::ids::InputLeaseId;
use fr_files::sender::Sender;
use fr_transport::quic::{
    ChannelScope, ConnectionBinding, ControlRoutes, Disposition, MediaChannel, QuicRecords, Route,
    files::FilesChannel,
};
use fr_wire::{
    attachment::{self, BINDING_RECORD_BYTES, MediaRole, Message},
    decoder::Binding,
    input::{InputDelivery, InputDirection},
    negotiation::{ControlBinding, Selection},
};
use std::time::Duration;

pub(super) struct Pending {
    cx: Cx,
    connection: ConnectionBinding,
    control: ControlRoutes,
    parent: ControlBinding,
    selection: Selection,
    expected: Binding,
    lease: InputLeaseId,
    handle: u128,
    permission: Permission,
    policy: Policy,
    until: u64,
    last: u64,
    channel: Option<MediaChannel>,
}
fn attachment_kind(bytes: &[u8]) -> bool {
    bytes
        .get(6..8)
        .is_some_and(|kind| (0x0018..=0x001c).contains(&u16::from_be_bytes([kind[0], kind[1]])))
}
impl Pending {
    pub(super) fn check(&mut self) -> Result<u64, Error> {
        self.cx.checkpoint().map_err(|_| Error::Cancelled)?;
        let now = self.cx.timer_driver().ok_or(Error::Clock)?.now().as_nanos() / 1000;
        if now < self.last {
            return Err(Error::Clock);
        }
        if now >= self.until {
            return Err(Error::Expired);
        }
        if !self.permission.is_approved() {
            return Err(Error::Cancelled);
        }
        self.last = now;
        Ok(now)
    }
    pub(super) fn deadline(&self) -> u64 {
        self.until
    }
    pub(super) fn owns(&self, route: Route, bytes: &[u8]) -> bool {
        (route==Route::Stream(self.control.inbound) && attachment_kind(bytes))
        || self.channel.as_ref().is_some_and(|channel|matches!(route,Route::Stream(r) if r.binding==channel.descriptor().binding.parent.id))
    }
    pub(super) fn advance(
        &mut self,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<bool, Error> {
        if !q.is_bound_to(&self.connection) {
            return Err(Error::WrongConnection);
        }
        self.check()?;
        if !authorize() {
            return Err(Error::Closed);
        }
        if self.channel.is_none() {
            self.accept(q, &mut authorize)?;
        }
        let Some(channel) = &mut self.channel else {
            return Ok(false);
        };
        channel
            .transmit(q, &self.cx, &mut authorize)
            .map_err(Error::Transport)?;
        channel
            .dispatch(q, &self.cx, &mut authorize)
            .map_err(Error::Transport)?;
        let complete = channel
            .finish(q, &self.cx, &mut authorize)
            .map_err(Error::Transport)?
            .is_some();
        self.check()?;
        Ok(complete)
    }
    fn accept(
        &mut self,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<(), Error> {
        let mut offer = [0; BINDING_RECORD_BYTES];
        let mut received = false;
        q.receive_ready(
            &self.cx,
            &mut authorize,
            |route| route == Route::Stream(self.control.inbound),
            |_, bytes| {
                if !attachment_kind(bytes) || received {
                    return Ok(Disposition::Blocked);
                }
                let Message::Binding(descriptor) = attachment::decode(
                    bytes,
                    self.parent,
                    self.parent.id,
                    &self.selection.limits,
                    InputDirection::HostToViewer,
                    InputDelivery::Reliable,
                )
                .map_err(|_| ())?
                else {
                    return Err(());
                };
                let expected = Binding {
                    parent: ControlBinding {
                        id: descriptor.binding.parent.id,
                        ..self.parent
                    },
                    ..self.expected
                };
                if descriptor.role != MediaRole::Files
                    || descriptor.binding != expected
                    || bytes.len() != offer.len()
                {
                    return Err(());
                }
                offer.copy_from_slice(bytes);
                received = true;
                Ok(Disposition::Consumed)
            },
        )
        .map_err(Error::Transport)?;
        if received {
            let remaining = self
                .until
                .checked_sub(self.check()?)
                .ok_or(Error::Expired)?;
            self.channel = Some(
                q.accept_media_channel(
                    &self.cx,
                    ChannelScope {
                        control: self.control,
                        parent: self.parent,
                        selection: &self.selection,
                    },
                    &offer,
                    Duration::from_micros(remaining),
                    authorize,
                )
                .map_err(Error::Transport)?,
            );
        }
        Ok(())
    }
    pub(super) fn start(mut self, q: &QuicRecords) -> Result<(Sender<'static>, Permission), Error> {
        if !q.is_bound_to(&self.connection) {
            return Err(Error::WrongConnection);
        }
        self.check()?;
        let channel = self.channel.take().ok_or(Error::Closed)?;
        let lane =
            FilesChannel::new(q, channel, self.lease, self.handle).map_err(Error::Transport)?;
        let sender = Sender::owning(self.cx, q, lane, self.policy)?;
        Ok((sender, self.permission))
    }
}
impl ControlledViewer {
    /// Expect exactly one host-initiated file attachment on the actual selected
    /// view. The handle is previously agreed local file scope, never a wire path.
    /// Timeout begins NOW, including idle time before the first drive. Ordinary
    /// controller turns accept/acknowledge/promote the channel; no alternate pump,
    /// connection, controller or source worker is created for this operation.
    pub fn expect_files(
        &mut self,
        handle: u128,
        permission: Permission,
        policy: Policy,
        timeout: Duration,
    ) -> Result<(), Error> {
        if self.files.used || self.files.stopped || self.clipboard_setup.negotiating() {
            return Err(Error::Busy);
        }
        self.files_admitted(&permission)?;
        if handle == 0 {
            return Err(Error::Wire(fr_wire::WireError::InvalidBinding));
        }
        let lifetime = u64::try_from(timeout.as_micros()).map_err(|_| Error::Limits)?;
        if !(1..=2_000_000).contains(&lifetime) {
            return Err(Error::Limits);
        }
        let cx = &self.session.cx;
        let last = cx.timer_driver().ok_or(Error::Clock)?.now().as_nanos() / 1000;
        let until = last.checked_add(lifetime).ok_or(Error::Clock)?;
        self.files.pending = Some(Pending {
            cx: cx.clone(),
            connection: self.session.transport.binding(),
            control: self.session.routes,
            parent: self.session.opened.binding,
            selection: self.session.opened.selection.clone(),
            expected: self.media.binding(),
            lease: self.input.binding().lease,
            handle,
            permission,
            policy,
            until,
            last,
            channel: None,
        });
        self.files.used = true;
        Ok(())
    }
    pub fn file_send_negotiating(&self) -> bool {
        self.files.pending.is_some()
    }
}
