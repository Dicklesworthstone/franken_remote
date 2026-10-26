//! One raw-admission controller for the original shared source. Per-peer
//! deadlines and retirement remain independent; one slow viewer never chooses
//! the rate of a healthy viewer. No codec parameter or recovery policy changes.
use super::{Entry, Error, MediaError, Members, ObservationControl, Publisher};
use crate::media::receiver_feedback::SharedSample;
use fr_media::pacing::{self, Availability, Controller, Observation, ReceiverEvidence, Sample};
use fr_wire::{decoder::Binding, receiver_metrics::Load};
use std::time::Duration;

#[derive(Clone, Copy)]
pub(super) struct SendSample {
    view: Binding,
    at: u64,
    availability: Availability,
}
impl SendSample {
    fn current(self, view: Binding) -> Option<(u64, Availability)> {
        (self.view == view).then_some((self.at, self.availability))
    }
}
fn record_send(
    slot: &mut Option<SendSample>,
    view: Binding,
    at: u64,
    progress: &crate::media_egress::Progress,
) {
    let availability = match progress {
        crate::media_egress::Progress::Accepted(_) => Availability::Ready,
        crate::media_egress::Progress::Pending(_) => Availability::Blocked,
        // No enqueue was attempted. Idle cannot refresh earlier evidence and
        // turn an arbitrarily silent peer into measured local headroom.
        crate::media_egress::Progress::Idle => return,
    };
    *slot = Some(SendSample {
        view,
        at,
        availability,
    });
}
impl Entry {
    pub(super) fn note_video_send(
        &mut self,
        progress: &crate::media_egress::Progress,
        owner: &ObservationControl,
    ) -> Result<(), Error> {
        record_send(
            &mut self.send_sample,
            self.view,
            owner.check().map_err(Error::Media)?.as_micros(),
            progress,
        );
        Ok(())
    }
}

/// Exactly one content-free snapshot. Only the original running session can
/// explicitly record an unnegotiated peer; absence of a report is unknown.
#[derive(Clone, Copy, Default)]
pub(super) enum ReceiverSample {
    #[default]
    Unknown,
    Unnegotiated(Binding),
    Measured(SharedSample),
}
impl ReceiverSample {
    fn current(self, view: Binding, now: u64) -> ReceiverEvidence {
        match self {
            Self::Unnegotiated(original) if original == view => ReceiverEvidence::Unobserved,
            Self::Measured(sample)
                if sample.view == view && sample.at_us <= now && now < sample.until_us =>
            {
                ReceiverEvidence::Measured(sample.load)
            }
            _ => ReceiverEvidence::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PeerEvidence {
    send: Availability,
    receiver: ReceiverEvidence,
}

pub(super) struct CapturePacing {
    controller: Controller,
    in_flight: Option<u64>,
    last_work: Option<(u64, u64)>,
    observation: Option<Observation>,
}
impl CapturePacing {
    fn new(minimum: Duration, maximum: Duration, fps: u16) -> Result<Self, Error> {
        if fps == 0
            || minimum > Duration::from_secs(1)
            || minimum.as_micros() < u128::from(1_000_000_u64.div_ceil(u64::from(fps)))
        {
            return Err(Error::InvalidBudget);
        }
        let policy = pacing::Policy {
            minimum_interval_us: u64::try_from(minimum.as_nanos().div_ceil(1000))
                .map_err(|_| Error::InvalidBudget)?,
            maximum_interval_us: u64::try_from(maximum.as_micros())
                .map_err(|_| Error::InvalidBudget)?,
        };
        Ok(Self {
            controller: Controller::new(policy).map_err(pacing_error)?,
            in_flight: None,
            last_work: None,
            observation: None,
        })
    }
    pub(super) fn interval_us(&self) -> u64 {
        self.controller.interval_us()
    }
    pub(super) fn start(&mut self, now: u64) -> Result<(), Error> {
        if self.in_flight.is_some() {
            return Err(Error::InvalidBudget);
        }
        self.in_flight = Some(now);
        Ok(())
    }
    pub(super) fn finish(
        &mut self,
        completed: u64,
        observed: u64,
        changed: bool,
    ) -> Result<(), Error> {
        let started = self.in_flight.ok_or(Error::InvalidBudget)?;
        if observed < started || observed > completed {
            return Err(pacing_error(pacing::Error::InvalidObservation));
        }
        self.last_work = Some((completed, elapsed(completed, started)?));
        self.in_flight = None;
        self.observation = Some(Observation {
            at_us: observed,
            changed,
        });
        Ok(())
    }
    pub(super) fn sample(
        &mut self,
        now: u64,
        pool_ready: bool,
        peers: PeerEvidence,
    ) -> Result<u64, Error> {
        let running = self.in_flight.map(|at| elapsed(now, at)).transpose()?;
        let completed = self
            .last_work
            .map(|(at, duration)| {
                elapsed(now, at).map(|age| (age <= pacing::SOURCE_EVIDENCE_US).then_some(duration))
            })
            .transpose()?
            .flatten();
        let capture_credit = if self.in_flight.is_some() {
            // Its own reservation is not evidence of pressure from other work.
            Availability::Unknown
        } else if pool_ready {
            Availability::Ready
        } else {
            Availability::Blocked
        };
        self.controller
            .update_with_receiver(
                Sample {
                    now_us: now,
                    source_work_us: running.into_iter().chain(completed).max(),
                    send: peers.send,
                    capture_credit,
                    observation: self.observation.take(),
                },
                peers.receiver,
            )
            .map(|report| report.interval_us)
            .map_err(pacing_error)
    }
}
fn elapsed(now: u64, earlier: u64) -> Result<u64, Error> {
    now.checked_sub(earlier)
        .ok_or_else(|| pacing_error(pacing::Error::ClockRegression))
}
fn pacing_error(error: pacing::Error) -> Error {
    Error::Media(MediaError::Pacing(error))
}

/// A probe needs one whole healthy path: this SAME viewer's accepted video
/// enqueue and live decoder headroom (or explicit absence of negotiation).
/// Otherwise receiver slowdown needs positive live pressure from EVERY eligible
/// viewer. Pick one complete Load, in stable subscriber order; never synthesize
/// a healthy receiver by combining counters from different viewers.
fn peer_evidence(
    now: u64,
    interval: u64,
    samples: impl Iterator<Item = (Option<(u64, Availability)>, ReceiverEvidence)>,
) -> PeerEvidence {
    let mut any = false;
    let mut any_ready = false;
    let mut all_blocked = true;
    let mut healthy = None;
    let mut pressured = None;
    let mut all_pressured = true;
    // At the slowest adaptive cadence a real enqueue occurs only every 200 ms.
    // Keep that outcome across one admission interval, bounded by the canonical
    // source-evidence horizon. Idle never refreshes the original timestamp.
    let send_lifetime = interval.clamp(100_000, pacing::SOURCE_EVIDENCE_US);
    for (send, receiver) in samples {
        any = true;
        let send = send
            .filter(|(at, _)| now.checked_sub(*at).is_some_and(|age| age <= send_lifetime))
            .map_or(Availability::Unknown, |(_, availability)| availability);
        any_ready |= send == Availability::Ready;
        all_blocked &= send == Availability::Blocked;
        let headroom = match receiver {
            ReceiverEvidence::Unobserved => true,
            ReceiverEvidence::Measured(load) => receiver_headroom(load, interval),
            ReceiverEvidence::Unknown => false,
        };
        if healthy.is_none() && send == Availability::Ready && headroom {
            healthy = Some(receiver);
        }
        if let ReceiverEvidence::Measured(load) = receiver
            && (load.retained_pictures > 1 || load.work_us.is_some_and(|work| work >= interval))
        {
            pressured.get_or_insert(receiver);
        } else {
            all_pressured = false;
        }
    }
    PeerEvidence {
        send: if any_ready {
            Availability::Ready
        } else if any && all_blocked {
            Availability::Blocked
        } else {
            Availability::Unknown
        },
        receiver: healthy
            .or(pressured.filter(|_| all_pressured))
            .unwrap_or(ReceiverEvidence::Unknown),
    }
}
fn receiver_headroom(load: Load, interval: u64) -> bool {
    // Match the canonical controller's measured-receiver headroom predicate.
    !load.decoding
        && load.retained_pictures <= 1
        && load.work_us.is_some_and(|work| work <= interval * 3 / 4)
}
impl Members {
    pub(super) fn peer_evidence(&self, now: u64, interval: u64) -> PeerEvidence {
        peer_evidence(
            now,
            interval,
            self.entries
                .iter()
                .flatten()
                .filter(|entry| {
                    entry.failure.is_none()
                        && entry.join.is_none()
                        && entry.starting.is_none()
                        && !entry.fenced()
                })
                .map(|entry| {
                    (
                        entry
                            .send_sample
                            .and_then(|sample| sample.current(entry.view)),
                        entry.receiver_sample.current(entry.view, now),
                    )
                }),
        )
    }
}
impl Publisher {
    /// Opt in before source service. The minimum is the SAME cadence passed to
    /// `serve`; the codec's configured fps remains a hard ceiling. The sole
    /// source, pool, native child, per-viewer authorities and reference chains
    /// remain unchanged. This is not bitrate or network-capacity adaptation.
    pub fn enable_adaptive_capture(
        &mut self,
        capture_interval: Duration,
        maximum: Duration,
    ) -> Result<(), Error> {
        self.members.lock().map_err(|_| Error::Poisoned)?.tick()?;
        if self.served || self.adaptive.is_some() {
            return Err(Error::InvalidBudget);
        }
        self.adaptive = Some(Box::new(CapturePacing::new(
            capture_interval,
            maximum,
            self.source.configuration.fps,
        )?));
        Ok(())
    }
    pub fn pacing(&self) -> Option<&Controller> {
        self.adaptive.as_ref().map(|pacing| &pacing.controller)
    }
    pub(super) fn capture_interval(&mut self, now: u64) -> Result<Option<u64>, Error> {
        let Some(adaptive) = &mut self.adaptive else {
            return Ok(None);
        };
        let capacity = usize::try_from(self.source.configuration.max_access_unit_bytes)
            .ok()
            .and_then(|n| n.checked_add(fr_media::worker::UNIT_PREFIX_BYTES))
            .ok_or(Error::InvalidBudget)?;
        let ready = self.pool.can_share_capacity(capacity);
        let peers = self
            .members
            .lock()
            .map_err(|_| Error::Poisoned)?
            .peer_evidence(now, adaptive.controller.interval_us());
        adaptive.sample(now, ready, peers).map(Some)
    }
}

#[cfg(test)]
mod tests;
