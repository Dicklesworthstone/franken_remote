//! One explicit request produces at most one input owner. No host response can
//! substitute for a local mapping acknowledgement or visible-frame evidence.
use crate::input::{ClientInstant, InputClient, Policy};
use fr_core::limits::ProtocolLimits;
use fr_media::freshness::ClockCorrelation;
use fr_wire::{
    WireError,
    control::{self, Granted, REQUEST_BYTES, Request},
    input::{InputDelivery, InputDirection},
    input_ticket::{self, INPUT_TICKET_BYTES},
};

pub const REQUEST_LIFETIME_US: u64 = 2_000_000;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidConfiguration,
    Wire(WireError),
    Input(crate::input::Error),
    WrongGrant,
    Order,
    Expired,
    Clock,
    Stopped,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Unsent,
    Sent,
    Done,
    Stopped,
}
/// The expected input channel must already have its authenticated routes on the
/// same session. This object negotiates a grant, not auxiliary-channel admission.
pub struct RequestControl {
    request: Request,
    channel: u32,
    limits: ProtocolLimits,
    bytes: [u8; REQUEST_BYTES],
    until: u64,
    last: u64,
    state: State,
}
impl RequestControl {
    pub fn new(
        request: Request,
        input_channel: u32,
        limits: ProtocolLimits,
        now: ClientInstant,
    ) -> Result<Self, Error> {
        if input_channel == 0
            || input_channel == request.parent.id
            || input_channel == request.target.display_binding
        {
            return Err(Error::InvalidConfiguration);
        }
        let until = now.0.checked_add(REQUEST_LIFETIME_US).ok_or(Error::Clock)?;
        let mut bytes = [0; REQUEST_BYTES];
        control::encode_request(
            request,
            &mut bytes,
            &limits,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .map_err(Error::Wire)?;
        Ok(Self {
            request,
            channel: input_channel,
            limits,
            bytes,
            until,
            last: now.0,
            state: State::Unsent,
        })
    }
    pub const fn deadline(&self) -> ClientInstant {
        ClientInstant(self.until)
    }
    pub fn stop(&mut self) {
        self.state = State::Stopped;
    }
    fn check(&mut self, now: ClientInstant) -> Result<(), Error> {
        if matches!(self.state, State::Stopped | State::Done) {
            return Err(Error::Stopped);
        }
        if now.0 < self.last {
            self.stop();
            return Err(Error::Clock);
        }
        self.last = now.0;
        if now.0 >= self.until {
            self.stop();
            return Err(Error::Expired);
        }
        Ok(())
    }
    /// Expose the original bytes while backpressured. Neither this call nor
    /// successful enqueue starts another request deadline.
    pub fn pending(&mut self, now: ClientInstant) -> Result<Option<&[u8]>, Error> {
        self.check(now)?;
        Ok((self.state == State::Unsent).then_some(self.bytes.as_slice()))
    }
    pub fn sent(&mut self, now: ClientInstant) -> Result<(), Error> {
        self.check(now)?;
        if self.state != State::Unsent {
            self.stop();
            return Err(Error::Order);
        }
        self.state = State::Sent;
        Ok(())
    }
    /// Accept only the exact request and the already installed input channel.
    /// Network clock evidence must come from THIS authenticated session. The
    /// returned owner still requires mapping and presentation, and carries the
    /// initial ticket's conservative local expiry from its first usable action.
    pub fn accept(
        &mut self,
        bytes: &[u8],
        clock: ClockCorrelation,
        policy: Policy,
        now: ClientInstant,
    ) -> Result<(Granted, InputClient), Error> {
        self.check(now)?;
        if self.state != State::Sent {
            self.stop();
            return Err(Error::Order);
        }
        // Any attempted grant consumes this request, including invalid, late,
        // or partially initialized results. There is no automatic reacquisition.
        self.state = State::Stopped;
        let grant = control::decode_granted(
            bytes,
            self.request.parent,
            &self.limits,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .map_err(Error::Wire)?;
        if grant.request != self.request
            || grant.input_channel != self.channel
            || clock.host_boot() != self.request.parent.host_boot
        {
            return Err(Error::WrongGrant);
        }
        let mut input = InputClient::new(
            grant.credentials(),
            self.channel,
            self.request.target.bounds,
            self.request.target.capabilities,
            self.limits,
            policy,
            now,
        )
        .map_err(Error::Input)?;
        let mut ticket = [0; INPUT_TICKET_BYTES];
        let n = input_ticket::encode(
            grant.initial_ticket(),
            &mut ticket,
            &self.limits,
            self.channel,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .map_err(Error::Wire)?;
        input
            .accept_ticket(&ticket[..n], clock, now)
            .map_err(Error::Input)?;
        self.state = State::Done;
        Ok((grant, input))
    }
}
