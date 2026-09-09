//! One correlated clock exchange at a time. Queueing, network transit and delayed
//! callbacks all remain inside the measured uncertainty interval. No host time
//! is compared directly to viewer time; no successful probe grants input.
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
        if self.stopped {
            return Err(Error::Stopped);
        }
        if now.0 < self.last_now {
            return self.fail(Error::ClockRegression);
        }
        self.last_now = now.0;
        if self.pending.as_ref().is_some_and(|p| now.0 >= p.until) {
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
        self.tick(now)?;
        let Some(pending) = &mut self.pending else {
            return self.fail(Error::UnexpectedReply);
        };
        if pending.queued {
            return self.fail(Error::UnexpectedReply);
        }
        pending.queued = true;
        Ok(())
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
