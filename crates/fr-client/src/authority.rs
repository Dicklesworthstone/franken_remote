//! A bounded observation-challenge responder. This keeps an already granted
//! observation alive; it never establishes capture freshness or input authority.
use crate::input::ClientInstant;
use fr_core::limits::ProtocolLimits;
use fr_wire::{
    WireError,
    authority::{
        self, Binding, Message, OBSERVATION_CHALLENGE_BYTES, OBSERVATION_RESPONSE_BYTES, Scope,
    },
    input::{InputDelivery, InputDirection},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidConfiguration,
    Stopped,
    Clock,
    Expired,
    Backpressure,
    StaleChallenge,
    WrongScope,
    Wire(WireError),
}
/// One unsent response. The session must stop this owner on focus/lifecycle or
/// disconnection boundaries required by its policy. A successful response is
/// neither fresh presentation evidence nor a control grant. Host time is opaque.
pub struct ObservationResponder {
    binding: Binding,
    limits: ProtocolLimits,
    clock: ClientInstant,
    nonce: Option<u128>,
    host_deadline: u64,
    until: Option<ClientInstant>,
    stopped: bool,
    bytes: [u8; OBSERVATION_RESPONSE_BYTES],
}
impl ObservationResponder {
    pub fn new(
        binding: Binding,
        limits: ProtocolLimits,
        now: ClientInstant,
    ) -> Result<Self, Error> {
        if binding.channel == 0
            || binding.session.as_raw() == 0
            || (limits.max_control_message_bytes() as usize) < OBSERVATION_CHALLENGE_BYTES
        {
            return Err(Error::InvalidConfiguration);
        }
        Ok(Self {
            binding,
            limits,
            clock: now,
            nonce: None,
            host_deadline: 0,
            until: None,
            stopped: false,
            bytes: [0; OBSERVATION_RESPONSE_BYTES],
        })
    }
    pub fn stop(&mut self) {
        self.stopped = true;
        self.until = None;
        self.bytes.fill(0);
    }
    pub const fn stopped(&self) -> bool {
        self.stopped
    }
    fn fail<T>(&mut self, error: Error) -> Result<T, Error> {
        self.stop();
        Err(error)
    }
    pub fn tick(&mut self, now: ClientInstant) -> Result<(), Error> {
        if self.stopped {
            return Err(Error::Stopped);
        }
        if now < self.clock {
            return self.fail(Error::Clock);
        }
        self.clock = now;
        if self.until.is_some_and(|until| now >= until) {
            return self.fail(Error::Expired);
        }
        Ok(())
    }
    /// Call only for an authenticated host control record. On Backpressure the
    /// transport retains the unread record; do not consume/drop it and continue.
    pub fn accept(&mut self, bytes: &[u8], now: ClientInstant) -> Result<(), Error> {
        self.tick(now)?;
        if self.until.is_some() {
            return Err(Error::Backpressure);
        }
        let message = match authority::decode(
            bytes,
            self.binding,
            &self.limits,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        ) {
            Ok(message) => message,
            Err(error) => return self.fail(Error::Wire(error)),
        };
        let Message::Challenge {
            scope: Scope::Observation,
            nonce,
            deadline_micros,
        } = message
        else {
            return self.fail(Error::WrongScope);
        };
        // Host issue-time deadlines advance at the renewal cadence. Replaying an
        // earlier challenge cannot replace the outstanding response or revive it.
        if self.nonce == Some(nonce) || deadline_micros <= self.host_deadline {
            return self.fail(Error::StaleChallenge);
        }
        let Some(until) = now.0.checked_add(1_000_000).map(ClientInstant) else {
            return self.fail(Error::Clock);
        };
        authority::encode(
            Message::Response {
                scope: Scope::Observation,
                nonce,
            },
            self.binding,
            &self.limits,
            &mut self.bytes,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .map_err(Error::Wire)?;
        self.nonce = Some(nonce);
        self.host_deadline = deadline_micros;
        self.until = Some(until);
        Ok(())
    }
    /// Original local send deadline, including time spent under backpressure.
    /// It is never a host authorization deadline or a fresh receipt-time TTL.
    pub const fn response_deadline(&self) -> Option<ClientInstant> {
        self.until
    }
    pub fn pending(&mut self, now: ClientInstant) -> Result<Option<&[u8]>, Error> {
        self.tick(now)?;
        Ok(self.until.map(|_| self.bytes.as_slice()))
    }
    /// Consume ONLY after the transport has accepted these exact bytes. A failed
    /// send does not change the deadline; transport failure must stop the session.
    pub fn sent(&mut self, now: ClientInstant) -> Result<(), Error> {
        self.tick(now)?;
        if self.until.take().is_none() {
            return self.fail(Error::StaleChallenge);
        }
        self.bytes.fill(0);
        Ok(())
    }
}
