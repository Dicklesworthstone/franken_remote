//! One optional attachment exchange on the original running control connection.
//! This owns bounded handshake metadata only. It never opens a native clipboard.
use crate::clipboard_quic::{Error, WorkerSeed};
use asupersync::cx::Cx;
use fr_core::clipboard::Binding as ClipboardBinding;
use fr_transport::quic::{
    ChannelRequest, ChannelScope, ConnectionBinding, ControlRoutes, Disposition, MediaChannel,
    QuicRecords, Route,
};
use fr_wire::clipboard::startup;
use fr_wire::{
    Kind,
    attachment::{self, BINDING_RECORD_BYTES, MediaRole, Message},
    decoder::Binding,
    input::{InputDelivery, InputDirection},
    negotiation::{ControlBinding, Role, Selection},
};
use std::time::Duration;

/// No restart after a successful start, including after consent refusal. The
/// transport also retains its consumed reservation; neither sequence floor resets.
#[derive(Default)]
pub(super) struct Setup {
    started: bool,
    pending: Option<Pending>,
    worker: Option<WorkerSeed>,
    reason: Option<Error>,
}
struct Pending {
    cx: Cx,
    connection: ConnectionBinding,
    parent: ControlBinding,
    control: ControlRoutes,
    selection: Selection,
    expected: Binding,
    until: u64,
    last: u64,
    consent: Consent,
    ready: Option<Ready>,
    channel: Option<MediaChannel>,
}
pub(super) struct Consent {
    pub scope: ClipboardBinding,
    pub granted: bool,
}
struct Ready {
    sent: bool,
    peer: Option<bool>,
}
fn selected(selection: &Selection) -> Result<(), Error> {
    if selection.role != Role::RequestControl
        || ![
            (attachment::CAPABILITY, attachment::VERSION),
            (attachment::INPUT_CAPABILITY, attachment::INPUT_VERSION),
            (
                attachment::CLIPBOARD_CAPABILITY,
                attachment::CLIPBOARD_VERSION,
            ),
            (fr_wire::clipboard::CAPABILITY, fr_wire::clipboard::VERSION),
            (startup::CAPABILITY, startup::VERSION),
        ]
        .iter()
        .all(|(name, version)| {
            selection
                .capabilities
                .iter()
                .any(|c| c.name == *name && c.version == *version)
        })
    {
        return Err(Error::NotNegotiated);
    }
    Ok(())
}
fn at(cx: &Cx) -> Result<u64, Error> {
    super::now(cx).map_err(|error| match error {
        super::Error::Cancelled => Error::Cancelled,
        _ => Error::Clock,
    })
}
fn deadline(cx: &Cx, timeout: Duration) -> Result<(u64, u64), Error> {
    let micros = u64::try_from(timeout.as_micros()).map_err(|_| Error::Limit)?;
    if !(1..=2_000_000).contains(&micros) {
        return Err(Error::Limit);
    }
    let start = at(cx)?;
    Ok((start, start.checked_add(micros).ok_or(Error::Clock)?))
}
fn attachment_kind(bytes: &[u8]) -> bool {
    bytes.get(6..8).is_some_and(|kind| {
        (0x0018..=0x001c).contains(&u16::from_be_bytes([kind[0], kind[1]]))
            || kind == (Kind::ClipboardReady as u16).to_be_bytes()
    })
}
impl Setup {
    pub(super) fn available(&self) -> bool {
        !self.started
    }
    pub(super) fn negotiating(&self) -> bool {
        self.pending.is_some()
    }
    pub(super) fn reason(&self) -> Option<Error> {
        self.reason
    }
    pub(super) fn take_worker(&mut self) -> Option<WorkerSeed> {
        self.worker.take()
    }
    pub(super) fn stop(&mut self) {
        self.pending = None;
        self.worker = None;
    }
    pub(super) fn joined(&mut self, result: Result<WorkerSeed, Error>) -> Result<(), Error> {
        match result {
            Ok(seed) => self.worker = Some(seed),
            Err(Error::ConsentRequired) => self.reason = Some(Error::ConsentRequired),
            Err(error) => return Err(error),
        }
        Ok(())
    }
    pub(super) fn offer(
        &mut self,
        cx: &Cx,
        q: &mut QuicRecords,
        scope: ChannelScope<'_>,
        request: ChannelRequest,
        consent: Consent,
        authorize: impl FnMut() -> bool,
    ) -> Result<(), Error> {
        if self.started {
            return Err(Error::AlreadyAttached);
        }
        selected(scope.selection)?;
        let (last, until) = deadline(cx, request.timeout)?;
        let channel = q
            .offer_media_role(cx, scope, request, MediaRole::Clipboard, authorize)
            .map_err(Error::Transport)?;
        self.pending = Some(Pending {
            cx: cx.clone(),
            connection: q.binding(),
            parent: scope.parent,
            control: scope.control,
            selection: scope.selection.clone(),
            expected: request.binding,
            until,
            last,
            consent,
            ready: None,
            channel: Some(channel),
        });
        self.started = true;
        Ok(())
    }
    pub(super) fn expect(
        &mut self,
        cx: &Cx,
        q: &QuicRecords,
        scope: ChannelScope<'_>,
        expected: Binding,
        timeout: Duration,
        consent: Consent,
    ) -> Result<(), Error> {
        if self.started {
            return Err(Error::AlreadyAttached);
        }
        selected(scope.selection)?;
        let (last, until) = deadline(cx, timeout)?;
        self.pending = Some(Pending {
            cx: cx.clone(),
            connection: q.binding(),
            parent: scope.parent,
            control: scope.control,
            selection: scope.selection.clone(),
            expected,
            until,
            last,
            consent,
            ready: None,
            channel: None,
        });
        self.started = true;
        Ok(())
    }
    /// Rechecked during actual UDP I/O, not only when handshake bytes are queued.
    pub(super) fn permits_io(&mut self) -> bool {
        self.pending.as_mut().is_none_or(|p| p.time().is_ok())
    }
    pub(super) fn deadline_us(&self) -> Option<u64> {
        self.pending.as_ref().map(|p| p.until)
    }
    pub(super) fn owns(&self, route: Route, bytes: &[u8]) -> bool {
        self.pending.as_ref().is_some_and(|p| {
            (route == Route::Stream(p.control.inbound) && attachment_kind(bytes))
                || p.channel.as_ref().is_some_and(|c| {
                    matches!(route, Route::Stream(r) if r.binding == c.descriptor().binding.parent.id)
                })
        })
    }
    /// At most one bounded descriptor and one transition per phase. No packet
    /// drive, sleep, native operation or application callback happens here.
    pub(super) fn service(
        &mut self,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<Option<(MediaChannel, bool)>, Error> {
        let Some(p) = &mut self.pending else {
            return Ok(None);
        };
        // Check identity BEFORE touching q; a replacement with equal IDs is not
        // the original connection. The containing session fences its own lifetime.
        if !q.is_bound_to(&p.connection) {
            return Err(Error::WrongConnection);
        }
        p.time()?;
        if !authorize() {
            return Err(Error::Closed);
        }
        if p.channel.is_none() {
            p.accept(q, &mut authorize)?;
        }
        if p.ready.is_none()
            && let Some(channel) = &mut p.channel
        {
            channel
                .transmit(q, &p.cx, &mut authorize)
                .map_err(Error::Transport)?;
            channel
                .dispatch(q, &p.cx, &mut authorize)
                .map_err(Error::Transport)?;
            if channel
                .finish(q, &p.cx, &mut authorize)
                .map_err(Error::Transport)?
                .is_some()
            {
                p.ready = Some(Ready {
                    sent: false,
                    peer: None,
                });
            }
        }
        if p.ready.is_some() && p.readiness(q, &mut authorize)? {
            p.time()?;
            let mut p = self.pending.take().ok_or(Error::Closed)?;
            let mut channel = p.channel.take().ok_or(Error::Closed)?;
            let consent = p.consent.granted && p.ready.is_some_and(|r| r.peer == Some(true));
            if !consent || channel.completed_on(q).is_err() {
                // Readiness lives on CONTROL, so resetting the optional stream
                // cannot destroy the peer's final handshake/consent receipt.
                // Also handles a crossed peer reset after native completion.
                channel
                    .retire_clipboard(q, &p.cx)
                    .map_err(Error::Transport)?;
                self.reason = Some(if consent {
                    Error::Closed
                } else {
                    Error::ConsentRequired
                });
                return Ok(None);
            }
            return Ok(Some((channel, true)));
        }
        Ok(None)
    }
}
impl Pending {
    fn time(&mut self) -> Result<u64, Error> {
        let current = at(&self.cx)?;
        if current < self.last {
            return Err(Error::Clock);
        }
        if current >= self.until {
            return Err(Error::SetupExpired);
        }
        self.last = current;
        Ok(current)
    }
    fn readiness(
        &mut self,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<bool, Error> {
        let channel = self
            .channel
            .as_ref()
            .ok_or(Error::Closed)?
            .descriptor()
            .binding
            .parent
            .id;
        let ready = self.ready.as_mut().ok_or(Error::Closed)?;
        if !ready.sent {
            let mut bytes = [0; startup::RECORD_BYTES];
            startup::encode(
                self.parent,
                self.consent.scope,
                channel,
                self.consent.granted,
                &self.selection.limits,
                &mut bytes,
            )
            .map_err(|_| Error::Transport(fr_transport::quic::Error::Malformed))?;
            match q.send(
                &self.cx,
                Route::Stream(self.control.outbound),
                &bytes,
                self.until,
                &mut authorize,
            ) {
                Ok(()) => ready.sent = true,
                Err(fr_transport::quic::Error::Backpressure) => {}
                Err(error) => return Err(Error::Transport(error)),
            }
        }
        q.receive_ready(
            &self.cx,
            &mut authorize,
            |route| route == Route::Stream(self.control.inbound),
            |_, bytes| {
                if bytes.get(6..8) != Some(&(Kind::ClipboardReady as u16).to_be_bytes()) {
                    return Ok(Disposition::Blocked);
                }
                if ready.peer.is_some() {
                    return Err(());
                }
                ready.peer = Some(
                    startup::decode(
                        bytes,
                        self.parent,
                        self.consent.scope,
                        channel,
                        &self.selection.limits,
                    )
                    .map_err(|_| ())?,
                );
                Ok(Disposition::Consumed)
            },
        )
        .map_err(Error::Transport)?;
        Ok(ready.sent && ready.peer.is_some())
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
                let Message::Binding(d) = attachment::decode(
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
                // The host may allocate a fresh binding ID, not change the selected
                // display or view tuple while attaching an auxiliary clipboard lane.
                let expected = Binding {
                    parent: ControlBinding {
                        id: d.binding.parent.id,
                        ..self.parent
                    },
                    ..self.expected
                };
                if d.role != MediaRole::Clipboard
                    || d.binding != expected
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
                .checked_sub(self.time()?)
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
}
