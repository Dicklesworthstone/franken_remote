//! One fixed-deadline role-8 expectation. No native device opens here.
use super::{Error, Factory, NativeMicrophone, State};
use asupersync::cx::Cx;
use fr_client::input::presentation::PresentedInput;
use fr_core::audio::MicPermission;
use fr_transport::quic::{
    ChannelScope, ConnectionBinding, ControlRoutes, Disposition, MediaChannel, QuicRecords, Route,
    audio::AudioUpChannel,
};
use fr_wire::{
    attachment::{self, BINDING_RECORD_BYTES, MediaRole, Message},
    decoder::Binding,
    input::{InputDelivery, InputDirection},
    negotiation::{ControlBinding, Selection},
};
use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};

/// Registered before even the first host offer arrives. Successful completion
/// binds the native guard once; failure never grants unconditional operation.
struct Gate {
    cx: Cx,
    started: u64,
    until: u64,
    native: OnceLock<Arc<dyn Fn() -> bool + Send + Sync>>,
}
impl Gate {
    fn permits(&self) -> bool {
        let Ok(now) = at(&self.cx) else {
            return false;
        };
        now >= self.started && self.native.get().map_or(now < self.until, |check| check())
    }
    fn bind(&self, check: Arc<dyn Fn() -> bool + Send + Sync>) -> Result<(), Error> {
        let now = at(&self.cx)?;
        if now < self.started {
            return Err(Error::Clock);
        }
        if now >= self.until {
            return Err(Error::SetupExpired);
        }
        self.native.set(check).map_err(|_| Error::AlreadyAttached)
    }
}
pub(super) struct Pending {
    cx: Cx,
    connection: ConnectionBinding,
    control: ControlRoutes,
    parent: ControlBinding,
    selection: Selection,
    expected: Binding,
    until: u64,
    last: u64,
    channel: Option<MediaChannel>,
    factory: Option<Factory>,
    gate: Arc<Gate>,
}
fn at(cx: &Cx) -> Result<u64, Error> {
    cx.checkpoint().map_err(|_| Error::Cancelled)?;
    Ok(cx.timer_driver().ok_or(Error::Clock)?.now().as_nanos() / 1000)
}
fn attachment_kind(bytes: &[u8]) -> bool {
    bytes
        .get(6..8)
        .is_some_and(|kind| (0x0018..=0x001c).contains(&u16::from_be_bytes([kind[0], kind[1]])))
}
fn selected(selection: &Selection, name: &str, version: u16) -> bool {
    selection
        .capabilities
        .iter()
        .any(|cap| cap.name == name && cap.version == version)
}
impl Pending {
    pub(super) fn new(
        cx: &Cx,
        q: &mut QuicRecords,
        scope: ChannelScope<'_>,
        expected: Binding,
        timeout: Duration,
        factory: Factory,
    ) -> Result<Self, Error> {
        let micros = u64::try_from(timeout.as_micros()).map_err(|_| Error::SetupExpired)?;
        if micros == 0 || micros > 2_000_000 || q.is_closed() {
            return Err(Error::SetupExpired);
        }
        let last = at(cx)?;
        let until = last.checked_add(micros).ok_or(Error::Clock)?;
        let gate = Arc::new(Gate {
            cx: cx.clone(),
            started: last,
            until,
            native: OnceLock::new(),
        });
        let retained = gate.clone();
        q.retain_operation_check(cx, Arc::new(move || retained.permits()))
            .map_err(Error::Transport)?;
        Ok(Self {
            cx: cx.clone(),
            connection: q.binding(),
            control: scope.control,
            parent: scope.parent,
            selection: scope.selection.clone(),
            expected,
            until,
            last,
            channel: None,
            factory: Some(factory),
            gate,
        })
    }
    pub(super) const fn deadline(&self) -> u64 {
        self.until
    }
    pub(super) fn check(&mut self) -> Result<u64, Error> {
        let now = at(&self.cx)?;
        if now < self.last {
            return Err(Error::Clock);
        }
        if now >= self.until {
            return Err(Error::SetupExpired);
        }
        self.last = now;
        Ok(now)
    }
    pub(super) fn owns(&self, route: Route, bytes: &[u8]) -> bool {
        if self.channel.as_ref().is_some_and(|channel| {
            matches!(route, Route::Stream(r) if r.binding == channel.descriptor().binding.parent.id)
        }) {
            return true;
        }
        route == Route::Stream(self.control.inbound)
            && attachment_kind(bytes)
            && attachment::decode(
                bytes,
                self.parent,
                self.parent.id,
                &self.selection.limits,
                InputDirection::HostToViewer,
                InputDelivery::Reliable,
            )
            .ok()
            .and_then(Message::descriptor)
            .is_some_and(|descriptor| descriptor.role == MediaRole::AudioUp)
    }
    pub(super) fn advance(
        &mut self,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<bool, Error> {
        if !q.is_bound_to(&self.connection) {
            return Err(Error::WrongBinding);
        }
        self.check()?;
        if let Some(channel) = &mut self.channel
            && channel
                .finish(q, &self.cx, &mut authorize)
                .map_err(Error::Transport)?
                .is_some()
        {
            self.check()?;
            return Ok(true);
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
        let ready = channel
            .finish(q, &self.cx, &mut authorize)
            .map_err(Error::Transport)?
            .is_some();
        self.check()?;
        Ok(ready)
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
                    Message::Binding(d) if d.role == MediaRole::AudioUp => d,
                    Message::Binding(d)
                    | Message::Ticket(attachment::Grant { descriptor: d, .. })
                        if foreign_selected(&self.selection, d.role) =>
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
                if descriptor.binding != expected || bytes.len() != offer.len() {
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
                .ok_or(Error::SetupExpired)?;
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
    ) -> Result<Box<dyn NativeMicrophone>, Error> {
        if !q.is_bound_to(&self.connection) {
            return Err(Error::WrongBinding);
        }
        self.check()?;
        let channel = self.channel.take().ok_or(Error::Closed)?;
        if channel.completed_parent(q).map_err(Error::Transport)? != self.parent {
            return Err(Error::WrongBinding);
        }
        let lane = AudioUpChannel::new(q, channel).map_err(Error::Transport)?;
        let factory = self.factory.take().ok_or(Error::Closed)?;
        let mut native = match factory(q, lane, input, &self.selection, &self.cx) {
            Ok(native) => native,
            Err(error) => {
                let (reason, mut lane) = error.into_parts();
                let _ = lane.retire(q, &self.cx);
                // Factory failure never changes this original expectation into
                // an unguarded connection; its completed lane stays consumed.
                q.close();
                return Err(reason);
            }
        };
        let status = native.status();
        if status.state != State::Muted || status.permission != MicPermission::NotRequested {
            native.stop();
            let _ = native.retire(q);
            q.close();
            return Err(Error::Order);
        }
        if let Err(error) = self.gate.bind(native.operation_check()) {
            native.stop();
            let _ = native.retire(q);
            q.close();
            return Err(error);
        }
        Ok(native)
    }
}
fn foreign_selected(selection: &Selection, role: MediaRole) -> bool {
    match role {
        MediaRole::Clipboard => {
            selected(
                selection,
                attachment::CLIPBOARD_CAPABILITY,
                attachment::CLIPBOARD_VERSION,
            ) && selected(selection, fr_wire::clipboard::CAPABILITY, 1)
        }
        MediaRole::Files => {
            selected(
                selection,
                attachment::FILES_CAPABILITY,
                attachment::FILES_VERSION,
            ) && selected(selection, fr_wire::files::CAPABILITY, 1)
        }
        MediaRole::FilesDownload => fr_wire::files::duplex_selected(selection),
        MediaRole::AudioDown => selected(selection, fr_wire::audio::CAPABILITY, 1),
        _ => false,
    }
}
