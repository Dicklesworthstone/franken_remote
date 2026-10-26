//! One local raw-admission controller for the original shared source. Per-peer
//! deadlines and retirement remain independent; one slow viewer never chooses
//! the rate of a healthy viewer. No codec parameter or recovery policy changes.
use super::{Entry, Error, MediaError, Members, ObservationControl, Publisher};
use fr_media::pacing::{self, Availability, Controller, Observation, Sample};
use fr_wire::decoder::Binding;
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
        send: Availability,
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
            .update(Sample {
                now_us: now,
                source_work_us: running.into_iter().chain(completed).max(),
                send,
                capture_credit,
                observation: self.observation.take(),
            })
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

/// Aggregate *actual video enqueue* samples. A healthy viewer can consume the
/// shared source independently of a laggard. Pressure requires every eligible
/// viewer to have recent blocked evidence; silence is never fabricated success.
fn send_evidence(
    now: u64,
    samples: impl Iterator<Item = Option<(u64, Availability)>>,
) -> Availability {
    let mut any = false;
    let mut all_blocked = true;
    for sample in samples {
        any = true;
        match sample.filter(|(at, _)| now.checked_sub(*at).is_some_and(|age| age <= 100_000)) {
            Some((_, Availability::Ready)) => return Availability::Ready,
            Some((_, Availability::Blocked)) => {}
            _ => all_blocked = false,
        }
    }
    if any && all_blocked {
        Availability::Blocked
    } else {
        Availability::Unknown
    }
}
impl Members {
    pub(super) fn send_evidence(&self, now: u64) -> Availability {
        send_evidence(
            now,
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
                    entry
                        .send_sample
                        .and_then(|sample| sample.current(entry.view))
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
        let send = self
            .members
            .lock()
            .map_err(|_| Error::Poisoned)?
            .send_evidence(now);
        adaptive.sample(now, ready, send).map(Some)
    }
}

#[cfg(test)]
mod tests;
