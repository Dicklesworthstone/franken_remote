//! Fresh role-specific media attachments on the original observation connection.
//! The old decoder/view must already be fenced. This owner neither grants input
//! nor configures a decoder: completion returns newly admitted media wiring.
use super::NegotiatedMedia;
use asupersync::{cx::Cx, net::quic_native::StreamRole};
use fr_transport::quic::{
    self, ChannelRequest, ChannelScope, ConnectionBinding, ControlRoutes, Disposition,
    MediaChannel, QuicRecords, Route,
};
use fr_wire::{
    attachment::{self, MediaRole, Message, Ticket},
    decoder::Binding,
    input::{InputDelivery, InputDirection},
    negotiation::{ControlBinding, Role, Selection},
};
use std::{cell::Cell, time::Duration};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Media(super::Error),
    Transport(quic::Error),
    WrongRole,
    WrongBinding,
    Expired,
    Closed,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

const ROLES: [MediaRole; 3] = [
    MediaRole::Configuration,
    MediaRole::Recovery,
    MediaRole::Video,
];

/// One replacement generation, three sequential one-use exchanges and ONE
/// absolute deadline. No auxiliary stream is opened by the application itself.
/// Advance between ordinary session turns so observation renewal remains owned
/// by the original session. Unrelated control records are left for that owner.
/// A partial exchange's abandonment retains the transport's terminal fence;
/// the parent must also close when abandoning a wait before its first offer.
pub struct Replacement {
    connection: ConnectionBinding,
    routes: ControlRoutes,
    parent: ControlBinding,
    selection: Selection,
    view: Binding,
    tickets: Option<[Ticket; 3]>,
    channels: [Option<MediaChannel>; 3],
    index: usize,
    until: u64,
    last: u64,
    closed: bool,
}
impl std::fmt::Debug for Replacement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaReplacement")
            .field("completed_roles", &self.index)
            .field("deadline", &self.until)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}
impl NegotiatedMedia {
    /// Retire old media only and negotiate its next recovery generation.
    /// `until` is the ORIGINAL failed-chain deadline, never a new timeout.
    /// The host supplies three distinct unpredictable tickets; the viewer uses
    /// None. Only observation-only sessions are supported: a controller needs a
    /// separate, authority-fenced reacquisition path, never implicit control.
    /// Call only after accepting/reporting the actual receiver failure.
    #[allow(clippy::too_many_arguments)]
    pub fn begin_replacement(
        self,
        cx: &Cx,
        q: &mut QuicRecords,
        control: ControlRoutes,
        parent: ControlBinding,
        until: u64,
        tickets: Option<[Ticket; 3]>,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<Replacement, Error> {
        // Refuse foreign connections before calling any mutating native API.
        if !q.is_bound_to(&self.connection) {
            return Err(Error::WrongBinding);
        }
        self.check_recovery_capability().map_err(Error::Media)?;
        if self.selection.role != Role::Observe
            || (q.role().map_err(Error::Transport)? == StreamRole::Server) != tickets.is_some()
        {
            return Err(Error::WrongRole);
        }
        let mut view = super::super::recovery::control_binding(
            q,
            control,
            parent,
            self.binding(),
            self.selection.limits,
        )
        .map_err(|_| Error::WrongBinding)?;
        view.recovery = view.recovery.next().ok_or(Error::WrongBinding)?;
        if q.remaining_channel_pairs() < 3 {
            return Err(Error::Transport(quic::Error::Backpressure));
        }
        let now = current(cx)?;
        if now >= until || until - now > 5_000_000 {
            return Err(Error::Expired);
        }
        if let Some(tickets) = &tickets {
            for (index, ticket) in tickets.iter().enumerate() {
                if ticket.0 == 0 || tickets[..index].contains(ticket) {
                    return Err(Error::WrongBinding);
                }
            }
        }
        cx.checkpoint()
            .map_err(|_| Error::Transport(quic::Error::Cancelled))?;
        if !authorize() {
            return Err(Error::Transport(quic::Error::Unauthorized));
        }
        if current(cx)? >= until {
            return Err(Error::Expired);
        }
        q.retire_media_set(
            cx,
            &self.connection,
            [self.configuration, self.recovery, self.video],
        )
        .map_err(Error::Transport)?;
        Ok(Replacement {
            connection: self.connection,
            routes: control,
            parent,
            selection: self.selection,
            view,
            tickets,
            channels: [None, None, None],
            index: 0,
            until,
            last: now,
            closed: false,
        })
    }
}
impl Replacement {
    /// Leave this exchange's records with its bounded transport slots while the
    /// parent dispatches renewal and unrelated services. Never route a partial
    /// attachment or early configuration payload to application callbacks.
    pub(crate) fn owns_record(&self, route: Route, bytes: &[u8]) -> bool {
        (route == Route::Stream(self.routes.inbound)
            && bytes.get(6..8).is_some_and(|k| {
                (0x0018..=0x001c).contains(&u16::from_be_bytes([k[0], k[1]]))
            }))
            || self.channels.iter().flatten().any(|channel| {
                matches!(route, Route::Stream(r) if r.binding == channel.descriptor().binding.parent.id)
            })
    }

    pub const fn deadline_micros(&self) -> u64 {
        self.until
    }
    pub const fn completed_roles(&self) -> usize {
        self.index
    }
    pub const fn is_complete(&self) -> bool {
        !self.closed && self.index == 3
    }

    /// One bounded dispatch/admission turn. Each existing child exchange uses
    /// the remaining budget; the final native authorization callback ALSO checks
    /// the original deadline, so fresh roles cannot extend an old failure.
    pub fn advance(
        &mut self,
        cx: &Cx,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<bool, Error> {
        if !q.is_bound_to(&self.connection) {
            return Err(Error::WrongBinding);
        }
        let result = self.advance_inner(cx, q, &mut authorize);
        if result.is_err() {
            self.closed = true;
            q.close();
        }
        result
    }
    fn check(&mut self, cx: &Cx, q: &QuicRecords) -> Result<u64, Error> {
        if self.closed || q.is_closed() {
            return Err(Error::Closed);
        }
        let now = current(cx)?;
        if now < self.last {
            return Err(Error::Transport(quic::Error::Clock));
        }
        self.last = now;
        if now >= self.until {
            return Err(Error::Expired);
        }
        cx.checkpoint()
            .map_err(|_| Error::Transport(quic::Error::Cancelled))?;
        Ok(now)
    }
    fn advance_inner(
        &mut self,
        cx: &Cx,
        q: &mut QuicRecords,
        authorize: &mut impl FnMut() -> bool,
    ) -> Result<bool, Error> {
        let now = self.check(cx, q)?;
        let until = self.until;
        let mut permitted = || current(cx).is_ok_and(|n| n >= now && n < until) && authorize();
        q.tick(cx, &mut permitted).map_err(Error::Transport)?;
        if self.index == 3 {
            return Ok(true);
        }
        if self.channels[self.index].is_none() {
            let timeout = Duration::from_micros((until - now).min(2_000_000));
            let scope = ChannelScope {
                control: self.routes,
                parent: self.parent,
                selection: &self.selection,
            };
            let channel = if let Some(tickets) = self.tickets {
                let mut binding = self.view;
                binding.parent.id = q.next_channel_binding().map_err(Error::Transport)?;
                q.offer_media_role(
                    cx,
                    scope,
                    ChannelRequest {
                        binding,
                        ticket: tickets[self.index],
                        timeout,
                    },
                    ROLES[self.index],
                    &mut permitted,
                )
                .map_err(Error::Transport)?
            } else {
                let Some((bytes, len)) = self.read_offer(cx, q, &mut permitted)? else {
                    return Ok(false);
                };
                q.accept_media_channel(cx, scope, &bytes[..len], timeout, &mut permitted)
                    .map_err(Error::Transport)?
            };
            self.channels[self.index] = Some(channel);
        }
        let channel = self.channels[self.index].as_mut().ok_or(Error::Closed)?;
        channel
            .transmit(q, cx, &mut permitted)
            .map_err(Error::Transport)?;
        channel
            .dispatch(q, cx, &mut permitted)
            .map_err(Error::Transport)?;
        if channel
            .finish(q, cx, &mut permitted)
            .map_err(Error::Transport)?
            .is_some()
        {
            self.index += 1;
        }
        self.check(cx, q)?;
        Ok(self.index == 3)
    }
    fn read_offer(
        &self,
        cx: &Cx,
        q: &mut QuicRecords,
        permitted: &mut impl FnMut() -> bool,
    ) -> Result<Option<([u8; attachment::GRANT_RECORD_BYTES], usize)>, Error> {
        let mut buffer = [0; attachment::GRANT_RECORD_BYTES];
        let mut len = 0;
        let ready = Cell::new(true);
        let mut failure = None;
        q.receive_ready(
            cx,
            permitted,
            |r| ready.get() && r == Route::Stream(self.routes.inbound),
            |_, bytes| {
                if bytes.get(6..8) != Some(&0x001b_u16.to_be_bytes()) {
                    return Ok(Disposition::Blocked);
                }
                let valid = (|| {
                    let Message::Binding(descriptor) = attachment::decode(
                        bytes,
                        self.parent,
                        self.parent.id,
                        &self.selection.limits,
                        InputDirection::HostToViewer,
                        InputDelivery::Reliable,
                    )
                    .map_err(|_| Error::WrongBinding)?
                    else {
                        return Err(Error::WrongBinding);
                    };
                    let mut binding = descriptor.binding;
                    binding.parent = self.parent;
                    if binding != self.view
                        || descriptor.role != ROLES[self.index]
                        || bytes.len() > buffer.len()
                    {
                        return Err(Error::WrongBinding);
                    }
                    len = bytes.len();
                    buffer[..len].copy_from_slice(bytes);
                    ready.set(false);
                    Ok(Disposition::Consumed)
                })();
                valid.map_err(|error| {
                    failure = Some(error);
                })
            },
        )
        .map_err(|e| failure.unwrap_or(Error::Transport(e)))?;
        Ok((len != 0).then_some((buffer, len)))
    }
    /// Move only the freshly completed set into the next decoder handshake.
    /// This does not reset the failure deadline: callers retain `deadline_micros`
    /// and cap configuration/decode on it. Dropping incomplete child exchanges
    /// still invokes their original abandonment fence.
    pub fn finish(mut self, cx: &Cx, q: &mut QuicRecords) -> Result<NegotiatedMedia, Error> {
        if !q.is_bound_to(&self.connection) {
            return Err(Error::WrongBinding);
        }
        let result = (|| {
            self.check(cx, q)?;
            if !self.is_complete() {
                return Err(Error::Closed);
            }
            let [Some(configuration), Some(recovery), Some(video)] = &self.channels else {
                return Err(Error::Closed);
            };
            NegotiatedMedia::new(q, &self.selection, configuration, recovery, video)
                .map_err(Error::Media)
        })();
        if result.is_err() {
            q.close();
        }
        result
    }
}
fn current(cx: &Cx) -> Result<u64, Error> {
    Ok(cx
        .timer_driver()
        .ok_or(Error::Transport(quic::Error::Clock))?
        .now()
        .as_nanos()
        / 1000)
}
