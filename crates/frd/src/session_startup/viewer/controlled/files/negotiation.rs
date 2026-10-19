//! One expected file-role attachment on the original viewer connection.
//! Configuration is bounded metadata; no source thread exists before `send_file`.
use super::{ControlledViewer, Error, Permission, Policy};
use crate::native_files::{ReceiveControl, ReceivePhase};
use asupersync::cx::Cx;
use fr_client::input::{ClientInstant, presentation::PresentedInput};
use fr_core::ids::InputLeaseId;
use fr_files::{quic::Configuration, sender::Sender};
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

pub(super) enum Destination {
    Send(Policy),
    Receive(Configuration, ReceiveControl),
}
pub(super) enum Advance {
    Waiting,
    Ready,
    PeerRetired,
}
pub(super) enum Attached {
    Cancelled,
    Sender(Box<Sender<'static>>, Permission),
    Receiver(Box<super::download::Receiving>, Permission),
}

pub(super) struct Pending {
    cx: Cx,
    connection: ConnectionBinding,
    control: ControlRoutes,
    parent: ControlBinding,
    selection: Selection,
    expected: Binding,
    lease: InputLeaseId,
    handle: Option<u128>,
    permission: Permission,
    destination: Destination,
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
    pub(super) fn is_download(&self) -> bool {
        matches!(self.destination, Destination::Receive(_, _))
    }
    pub(super) fn stopped(&self) {
        if let Destination::Receive(_, control) = &self.destination {
            control.end(crate::native_files::Absence::Refused(Error::Cancelled));
            control.cleaned(Ok(()));
        }
    }
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
    pub(super) fn preflight(
        &mut self,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<Advance, Error> {
        if !q.is_bound_to(&self.connection) {
            return Err(Error::WrongConnection);
        }
        self.check()?;
        if !authorize() {
            return Err(Error::Closed);
        }
        let download = self.is_download();
        let Some(channel) = &mut self.channel else {
            return Ok(Advance::Waiting);
        };
        if download
            && channel
                .retire_files_if_peer_cancelled(q, &self.cx, &mut authorize)
                .map_err(Error::Transport)?
        {
            return Ok(Advance::PeerRetired);
        }
        let complete = channel
            .finish(q, &self.cx, &mut authorize)
            .map_err(Error::Transport)?
            .is_some();
        self.check()?;
        Ok(if complete {
            Advance::Ready
        } else {
            Advance::Waiting
        })
    }
    pub(super) fn advance(
        &mut self,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<Advance, Error> {
        match self.preflight(q, &mut authorize)? {
            Advance::Waiting => {}
            outcome => return Ok(outcome),
        }
        if self.channel.is_none() {
            self.accept(q, &mut authorize)?;
        }
        let Some(channel) = &mut self.channel else {
            return Ok(Advance::Waiting);
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
        Ok(if complete {
            Advance::Ready
        } else {
            Advance::Waiting
        })
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
                let message = attachment::decode(
                    bytes,
                    self.parent,
                    self.parent.id,
                    &self.selection.limits,
                    InputDirection::HostToViewer,
                    InputDelivery::Reliable,
                )
                .map_err(|_| ())?;
                let descriptor = match message {
                    Message::Binding(d) if d.role == MediaRole::Files => d,
                    Message::Binding(d)
                    | Message::Ticket(attachment::Grant { descriptor: d, .. })
                        if d.role == MediaRole::Clipboard =>
                    {
                        return Ok(Disposition::Blocked);
                    }
                    _ => return Err(()),
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
    pub(super) fn start(
        mut self,
        q: &mut QuicRecords,
        input: &mut PresentedInput,
    ) -> Result<Attached, Error> {
        if !q.is_bound_to(&self.connection) {
            return Err(Error::WrongConnection);
        }
        let now = self.check()?;
        let channel = self.channel.take().ok_or(Error::Closed)?;
        // The scope comes from the exact completed attachment, never a path.
        let id = channel.descriptor().binding.parent.id;
        let handle = self.handle.unwrap_or_else(|| u128::from(id));
        let mut lane =
            FilesChannel::new(q, channel, self.lease, handle).map_err(Error::Transport)?;
        match self.destination {
            Destination::Send(policy) => {
                let sender = Sender::owning(self.cx, q, lane, policy)?;
                Ok(Attached::Sender(Box::new(sender), self.permission))
            }
            Destination::Receive(configuration, control) => {
                if control.cancel_requested() {
                    lane.retire(q, &self.cx).map_err(Error::Transport)?;
                    control.end(crate::native_files::Absence::Refused(Error::Cancelled));
                    control.cleaned(Ok(()));
                    return Ok(Attached::Cancelled);
                }
                let authority = input
                    .file_receive_authority(self.parent, id, self.lease, ClientInstant(now))
                    .map_err(|_| Error::Cancelled)?;
                let receiver = fr_files::quic::Receiver::spawn_with_authority(
                    self.cx,
                    q,
                    lane,
                    fr_files::session::Authority::from_controller(authority),
                    configuration,
                )
                .map_err(|error| {
                    control.end(crate::native_files::Absence::ReceiveFailed(error));
                    super::download::send_error(error)
                })?;
                control.set_phase(ReceivePhase::Receiving);
                Ok(Attached::Receiver(
                    Box::new(super::download::Receiving::new(receiver, control)),
                    self.permission,
                ))
            }
        }
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
        self.expect_scope(Some(handle), permission, policy, timeout)
    }
    /// Accept a locally approved host drop lane without a prearranged handle.
    /// Requires positive `file-channel-scope` v1 negotiation. The authenticated
    /// completed Files binding names the host's separately approved directory;
    /// it does not permit browsing, arbitrary paths, or observer file access.
    /// No local descriptor is opened or read until explicit `send_file`.
    pub fn expect_file_drop(
        &mut self,
        permission: Permission,
        policy: Policy,
        timeout: Duration,
    ) -> Result<(), Error> {
        self.files_admitted(&permission)?;
        if !self
            .session
            .opened
            .selection
            .capabilities
            .iter()
            .any(|cap| {
                cap.name == fr_wire::files::CHANNEL_SCOPE_CAPABILITY
                    && cap.version == fr_wire::files::CHANNEL_SCOPE_VERSION
            })
        {
            return Err(Error::WrongRole);
        }
        self.expect_scope(None, permission, policy, timeout)
    }
    fn expect_scope(
        &mut self,
        handle: Option<u128>,
        permission: Permission,
        policy: Policy,
        timeout: Duration,
    ) -> Result<(), Error> {
        if self
            .session
            .opened
            .selection
            .capabilities
            .iter()
            .any(|cap| {
                cap.name == fr_wire::files::DOWNLOAD_CAPABILITY
                    && cap.version == fr_wire::files::DOWNLOAD_VERSION
            })
        {
            return Err(Error::WrongRole);
        }
        self.expect_file_target(handle, permission, Destination::Send(policy), timeout)
    }
    pub(super) fn expect_file_target(
        &mut self,
        handle: Option<u128>,
        permission: Permission,
        destination: Destination,
        timeout: Duration,
    ) -> Result<(), Error> {
        if self.files.used || self.files.stopped {
            return Err(Error::Busy);
        }
        self.files_admitted(&permission)?;
        if handle == Some(0) {
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
            expected: self.media.as_ref().ok_or(Error::Closed)?.binding(),
            lease: self.input.binding().lease,
            handle,
            permission,
            destination,
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
