//! One correlated clock exchange at a time. Queueing, network transit and delayed
//! callbacks all remain inside the measured uncertainty interval. No host time
//! is compared directly to viewer time; no successful probe grants input.
//!
//! A probe whose reply misses `max_exchange_us` is given up, not fatal: it
//! measured nothing usable, the last correlation stays usable until its own
//! fixed expiry, and the next probe measures again. The probe shares the
//! control stream with the session's video, so on a saturated link one reply
//! can queue past a second; that used to end the whole session
//! (`ClockSynchronization`) although the last correlation had nine seconds of
//! validity left. A host that answers no probe for a whole correlation lifetime
//! (`valid_for_us`) still ends the exchange with `Expired`.
use crate::input::ClientInstant;
use fr_core::limits::ProtocolLimits;
use fr_media::freshness::{ClockCorrelation, ClockPolicy, ClockSample};
use fr_wire::{
    WireError,
    clock::{self, Message, PROBE_BYTES},
    input::{InputDelivery, InputDirection},
    negotiation::ControlBinding,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Configuration,
    Stopped,
    ClockRegression,
    ClockOverflow,
    Expired,
    Busy,
    UnexpectedReply,
    HostClockRegression,
    /// The reply of a probe already given up. Not a failure: the exchange
    /// continues, and the caller consumes the record without a correlation.
    LateReply,
    Wire(WireError),
    Correlation(fr_media::freshness::Error),
}
struct Pending {
    sequence: u64,
    started: u64,
    until: u64,
    queued: bool,
}
/// The transport adapter must bind this non-cloneable owner to one admitted
/// connection lifetime. Reinitializing it on the same channel permits replay;
/// a native adapter must enforce a sticky single attachment per connection.
pub struct ClockExchange {
    binding: ControlBinding,
    limits: ProtocolLimits,
    policy: ClockPolicy,
    last_now: u64,
    next_sequence: Option<u64>,
    pending: Option<Pending>,
    bytes: [u8; PROBE_BYTES],
    correlation: Option<ClockCorrelation>,
    last_host: Option<u64>,
    /// The highest sequence of a sent probe that was given up. Its reply still
    /// arrives on the reliable stream, after which it is ignored.
    abandoned: Option<u64>,
    /// When the exchange started or last accepted a reply.
    heard: u64,
    stopped: bool,
}
impl ClockExchange {
    pub fn new(
        binding: ControlBinding,
        limits: ProtocolLimits,
        policy: ClockPolicy,
        now: ClientInstant,
    ) -> Result<Self, Error> {
        // Validate local policy without publishing a fabricated correlation.
        ClockCorrelation::new(
            ClockSample {
                host_boot: binding.host_boot,
                client_sent_us: now.0,
                host_sample_us: 0,
                client_received_us: now.0,
            },
            policy,
        )
        .map_err(Error::Correlation)?;
        if binding.id == 0
            || binding.os_session.as_raw() == 0
            || binding.remote_session.as_raw() == 0
            || (limits.max_control_message_bytes() as usize) < clock::REPLY_BYTES
        {
            return Err(Error::Configuration);
        }
        Ok(Self {
            binding,
            limits,
            policy,
            last_now: now.0,
            next_sequence: Some(1),
            pending: None,
            bytes: [0; PROBE_BYTES],
            correlation: None,
            last_host: None,
            abandoned: None,
            heard: now.0,
            stopped: false,
        })
    }
    pub fn stop(&mut self) {
        self.stopped = true;
        self.pending = None;
        self.correlation = None;
        self.bytes.fill(0);
    }
    fn fail<T>(&mut self, error: Error) -> Result<T, Error> {
        self.stop();
        Err(error)
    }
    pub fn tick(&mut self, now: ClientInstant) -> Result<(), Error> {
        self.observe(now)?;
        self.expire(now.0)
    }
    fn observe(&mut self, now: ClientInstant) -> Result<(), Error> {
        if self.stopped {
            return Err(Error::Stopped);
        }
        if now.0 < self.last_now {
            return self.fail(Error::ClockRegression);
        }
        self.last_now = now.0;
        Ok(())
    }
    /// Give up a probe past its exchange bound. A sent one reaches the host on
    /// the reliable stream, so its reply is expected and ignored later; an
    /// unsent one never left, so the next probe reuses its sequence and the
    /// host still sees consecutive sequences.
    fn expire(&mut self, now: u64) -> Result<(), Error> {
        if let Some(pending) = self.pending.take_if(|p| now >= p.until) {
            if pending.queued {
                self.abandoned = Some(pending.sequence);
            } else {
                self.next_sequence = Some(pending.sequence);
            }
            self.bytes.fill(0);
        }
        if now.saturating_sub(self.heard) >= self.policy.valid_for_us {
            return self.fail(Error::Expired);
        }
        Ok(())
    }
    /// Record the lower endpoint BEFORE attempting any transport enqueue. Failed
    /// admission retains these exact bytes and this original timestamp.
    pub fn begin(&mut self, now: ClientInstant) -> Result<(), Error> {
        self.tick(now)?;
        if self.pending.is_some() {
            return Err(Error::Busy);
        }
        let Some(sequence) = self.next_sequence else {
            return self.fail(Error::ClockOverflow);
        };
        let Some(until) = now.0.checked_add(self.policy.max_exchange_us) else {
            return self.fail(Error::ClockOverflow);
        };
        clock::encode(
            Message::Probe { sequence },
            self.binding,
            &self.limits,
            &mut self.bytes,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .map_err(Error::Wire)?;
        self.next_sequence = sequence.checked_add(1);
        self.pending = Some(Pending {
            sequence,
            started: now.0,
            until,
            queued: false,
        });
        Ok(())
    }
    pub fn pending(&mut self, now: ClientInstant) -> Result<Option<(&[u8], u64)>, Error> {
        self.tick(now)?;
        Ok(self
            .pending
            .as_ref()
            .filter(|p| !p.queued)
            .map(|p| (self.bytes.as_slice(), p.until)))
    }
    /// Call only after this exact probe entered the authenticated transport.
    /// This does not resample its start time or establish a clock correlation.
    pub fn queued(&mut self, now: ClientInstant) -> Result<(), Error> {
        // Mark it sent before any expiry: a probe that just entered the
        // transport must never have its sequence reused.
        self.observe(now)?;
        let Some(pending) = &mut self.pending else {
            return self.fail(Error::UnexpectedReply);
        };
        if pending.queued {
            return self.fail(Error::UnexpectedReply);
        }
        pending.queued = true;
        self.expire(now.0)
    }
    /// The receive timestamp is sampled after the actual reply is available.
    /// Early, duplicate, unsolicited and other-session replies never create data.
    pub fn accept(&mut self, bytes: &[u8], now: ClientInstant) -> Result<ClockCorrelation, Error> {
        self.tick(now)?;
        let message = match clock::decode(
            bytes,
            self.binding,
            &self.limits,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        ) {
            Ok(message) => message,
            Err(e) => return self.fail(Error::Wire(e)),
        };
        let Message::Reply {
            sequence,
            host_sample_us,
        } = message
        else {
            return self.fail(Error::UnexpectedReply);
        };
        // Replies arrive in probe order, so one at or below the last given-up
        // sequence (and not the probe in flight) is that late reply.
        if self
            .abandoned
            .is_some_and(|abandoned| sequence <= abandoned)
            && self.pending.as_ref().is_none_or(|p| p.sequence != sequence)
        {
            return Err(Error::LateReply);
        }
        let Some(pending) = &self.pending else {
            return self.fail(Error::UnexpectedReply);
        };
        if !pending.queued || sequence != pending.sequence {
            return self.fail(Error::UnexpectedReply);
        }
        if self
            .last_host
            .is_some_and(|previous| host_sample_us < previous)
        {
            return self.fail(Error::HostClockRegression);
        }
        let correlation = match ClockCorrelation::new(
            ClockSample {
                host_boot: self.binding.host_boot,
                client_sent_us: pending.started,
                host_sample_us,
                client_received_us: now.0,
            },
            self.policy,
        ) {
            Ok(correlation) => correlation,
            Err(e) => return self.fail(Error::Correlation(e)),
        };
        self.last_host = Some(host_sample_us);
        self.correlation = Some(correlation);
        self.pending = None;
        self.heard = now.0;
        self.bytes.fill(0);
        Ok(correlation)
    }
    /// A previous successful exchange stays usable during a bounded refresh,
    /// but its original validity never slides and an expired result is not fresh.
    pub fn correlation(&mut self, now: ClientInstant) -> Result<Option<ClockCorrelation>, Error> {
        self.tick(now)?;
        Ok(self.correlation.filter(|c| now.0 < c.valid_until_us()))
    }
    pub fn in_flight(&self) -> bool {
        self.pending.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::ids::{HostBootId, OsSessionId, RemoteSessionId};

    const LIMITS: ProtocolLimits = ProtocolLimits::ABSOLUTE;
    const START: u64 = 1_000;

    fn binding() -> ControlBinding {
        ControlBinding {
            id: 17,
            host_boot: HostBootId::from_raw(1),
            os_session: OsSessionId::from_raw(2),
            remote_session: RemoteSessionId::from_raw(3),
        }
    }
    fn exchange(max_exchange_us: u64, valid_for_us: u64) -> ClockExchange {
        let policy = ClockPolicy {
            max_exchange_us,
            valid_for_us,
            ..ClockPolicy::default()
        };
        ClockExchange::new(binding(), LIMITS, policy, ClientInstant(START)).unwrap()
    }
    fn send(exchange: &mut ClockExchange, now: u64) -> u64 {
        exchange.begin(ClientInstant(now)).unwrap();
        let (bytes, _) = exchange.pending(ClientInstant(now)).unwrap().unwrap();
        let Message::Probe { sequence } = clock::decode(
            bytes,
            binding(),
            &LIMITS,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap() else {
            panic!("a probe");
        };
        exchange.queued(ClientInstant(now)).unwrap();
        sequence
    }
    fn reply(sequence: u64, host_sample_us: u64) -> [u8; clock::REPLY_BYTES] {
        let mut bytes = [0; clock::REPLY_BYTES];
        clock::encode(
            Message::Reply {
                sequence,
                host_sample_us,
            },
            binding(),
            &LIMITS,
            &mut bytes,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .unwrap();
        bytes
    }

    #[test]
    fn a_probe_past_its_bound_is_given_up_and_its_late_reply_ignored() {
        let mut exchange = exchange(250_000, 10_000_000);
        assert_eq!(send(&mut exchange, START), 1);
        let late = START + 300_000;
        exchange.tick(ClientInstant(late)).unwrap();
        assert!(!exchange.in_flight());
        assert_eq!(send(&mut exchange, late), 2);
        assert_eq!(
            exchange.accept(&reply(1, 50_000), ClientInstant(late + 10)),
            Err(Error::LateReply)
        );
        assert!(
            exchange.in_flight(),
            "the late reply leaves probe 2 waiting"
        );
        let correlation = exchange
            .accept(&reply(2, 60_000), ClientInstant(late + 20))
            .unwrap();
        assert_eq!(correlation.received_at_us(), late + 20);
        assert_eq!(
            exchange.correlation(ClientInstant(late + 30)).unwrap(),
            Some(correlation)
        );
    }

    #[test]
    fn an_unsent_probe_past_its_bound_keeps_its_sequence() {
        let mut exchange = exchange(250_000, 10_000_000);
        exchange.begin(ClientInstant(START)).unwrap();
        // Backpressure: the probe never entered the transport.
        exchange.tick(ClientInstant(START + 300_000)).unwrap();
        assert!(!exchange.in_flight());
        assert_eq!(send(&mut exchange, START + 300_000), 1);
    }

    #[test]
    fn a_host_that_answers_nothing_for_a_whole_correlation_lifetime_ends_the_exchange() {
        let mut exchange = exchange(250_000, 1_000_000);
        let mut now = START;
        for _ in 0..3 {
            send(&mut exchange, now);
            now += 300_000;
            exchange.tick(ClientInstant(now)).unwrap();
        }
        assert_eq!(
            exchange.tick(ClientInstant(START + 1_000_000)),
            Err(Error::Expired)
        );
        assert_eq!(
            exchange.tick(ClientInstant(START + 1_000_001)),
            Err(Error::Stopped)
        );
    }

    #[test]
    fn each_accepted_reply_restarts_the_silence_bound() {
        let mut exchange = exchange(250_000, 1_000_000);
        let sequence = send(&mut exchange, START + 900_000);
        exchange
            .accept(&reply(sequence, 10), ClientInstant(START + 950_000))
            .unwrap();
        // Past START + valid_for, but within it of the last reply.
        exchange.tick(ClientInstant(START + 1_500_000)).unwrap();
        assert_eq!(
            exchange.tick(ClientInstant(START + 1_950_000)),
            Err(Error::Expired)
        );
    }
}
