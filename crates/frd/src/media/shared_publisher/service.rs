//! Cadenced source service, independent of each subscriber's network task.
use super::{CaptureReport, Error, MediaError, Publisher};
use asupersync::{time::sleep_until, types::Time};
use std::{future::Future, time::Duration};

const MAINTENANCE_US: u64 = 10_000;

#[derive(Debug, Clone, Copy)]
struct Cadence {
    interval: u64,
    next: u64,
}
impl Cadence {
    fn new(interval: Duration, fps: u16, now: u64) -> Result<Self, Error> {
        if fps == 0 || interval > Duration::from_secs(1) {
            return Err(Error::InvalidBudget);
        }
        let minimum = 1_000_000_u64.div_ceil(u64::from(fps));
        if interval.as_micros() < u128::from(minimum) {
            return Err(Error::InvalidBudget);
        }
        // Round UP: a fractional microsecond must not exceed the admitted fps.
        let interval =
            u64::try_from(interval.as_nanos().div_ceil(1000)).map_err(|_| Error::InvalidBudget)?;
        let next = now.checked_add(interval).ok_or(Error::InvalidBudget)?;
        Ok(Self { interval, next })
    }
    fn after_turn(&mut self, now: u64) -> Result<(), Error> {
        // Backpressure, slow capture and a delayed callback discard missed raw
        // opportunities. Never accumulate a catch-up burst of codec work.
        self.next = now.checked_add(self.interval).ok_or(Error::Closed)?;
        Ok(())
    }
    fn wake(&self, now: u64, media_deadline: Option<u64>) -> Result<u64, Error> {
        let maintenance = now.checked_add(MAINTENANCE_US).ok_or(Error::Closed)?;
        Ok(self
            .next
            .min(maintenance)
            .min(media_deadline.unwrap_or(u64::MAX)))
    }
}

impl Publisher {
    /// Continuously service the ONE original capture source. Each Subscriber's
    /// sibling task still drives its original connection, renewal and repairs;
    /// this task never waits for all connections or acquires their transport.
    ///
    /// Capture cadence cannot exceed the source's configured frame rate. A slow
    /// native completion or full pool never causes catch-up captures. During idle
    /// waits, source/recipient expiry and last-subscriber departure are scheduled for
    /// checking within 10 ms, with earlier media deadlines taking precedence. These
    /// timer checks are not observations and do not renew any permission.
    ///
    /// `report` is a synchronous, bounded admission-statistics callback; it receives
    /// no pixels, connection, authority grant or source ownership. It runs outside
    /// policy locks. A callback which takes time cannot create another burst.
    /// Source consent is rechecked immediately afterward. Dropping this future,
    /// including before its first poll, fences the cohort and aborts the original
    /// child. Keep the Publisher to confirm cleanup through `reap`.
    pub fn serve<'a>(
        &'a mut self,
        capture_interval: Duration,
        mut report: impl FnMut(CaptureReport) + 'a,
    ) -> impl Future<Output = Result<(), Error>> + 'a {
        // Established at CALL time, not inside the async block.
        let operation = ServiceOperation { publisher: self };
        async move {
            let publisher = &mut *operation.publisher;
            let (owner, mut cadence) = {
                let mut members = publisher.members.lock().map_err(|_| Error::Poisoned)?;
                members.tick()?;
                if members.active() == 0 {
                    return Err(Error::NoSubscribers);
                }
                (
                    members.owner.clone(),
                    Cadence::new(
                        capture_interval,
                        publisher.source.configuration.fps,
                        members.last,
                    )?,
                )
            };
            loop {
                let (now, deadline) = {
                    let mut members = publisher.members.lock().map_err(|_| Error::Poisoned)?;
                    members.tick()?;
                    let deadline = members
                        .entries
                        .iter()
                        .flatten()
                        .filter(|e| e.failure.is_none())
                        .flat_map(|e| {
                            e.sender
                                .next_deadline()
                                .map(fr_core::time::HostInstant::as_micros)
                                .into_iter()
                                .chain(e.starting.as_ref().map(|s| s.host.deadline_us()))
                        })
                        .min();
                    (members.last, deadline)
                };
                if now < cadence.next {
                    let wake = cadence.wake(now, deadline)?;
                    let nanos = wake.checked_mul(1000).ok_or(Error::Closed)?;
                    // A fresh Sleep per wait, never repoll a completed timer.
                    sleep_until(Time::from_nanos(nanos)).await;
                    continue;
                }
                match publisher.capture_inner().await {
                    Ok(update) => report(update),
                    Err(Error::Media(MediaError::Backpressure)) => {}
                    Err(error) => return Err(error),
                }
                cadence.after_turn(owner.check().map_err(Error::Media)?.as_micros())?;
            }
        }
    }
}
struct ServiceOperation<'a> {
    publisher: &'a mut Publisher,
}
impl Drop for ServiceOperation<'_> {
    fn drop(&mut self) {
        self.publisher.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cadence_cannot_exceed_codec_fps_or_round_a_fraction_down() {
        for duration in [
            Duration::ZERO,
            Duration::from_micros(33_333),
            Duration::from_secs(2),
        ] {
            assert!(matches!(
                Cadence::new(duration, 30, 0),
                Err(Error::InvalidBudget)
            ));
        }
        assert!(Cadence::new(Duration::from_secs(1), 0, 0).is_err());
        let cadence = Cadence::new(Duration::from_nanos(33_334_001), 30, 1).unwrap();
        assert_eq!((cadence.interval, cadence.next), (33_335, 33_336));
    }
    #[test]
    fn stalled_and_backpressured_turns_do_not_accumulate_catchup_work() {
        let mut cadence = Cadence::new(Duration::from_millis(50), 30, 0).unwrap();
        cadence.after_turn(900_000).unwrap();
        assert_eq!(cadence.next, 950_000);
        cadence.after_turn(1_950_000).unwrap();
        assert_eq!(cadence.next, 2_000_000);
    }
    #[test]
    fn idle_wait_services_earlier_media_deadlines_and_bounded_consent_checks() {
        let cadence = Cadence::new(Duration::from_secs(1), 30, 0).unwrap();
        assert_eq!(cadence.wake(0, None).unwrap(), MAINTENANCE_US);
        assert_eq!(cadence.wake(0, Some(1234)).unwrap(), 1234);
        assert_eq!(cadence.wake(999_999, None).unwrap(), 1_000_000);
    }
    #[test]
    fn overflow_refuses_instead_of_wrapping_into_an_immediate_timer() {
        assert!(Cadence::new(Duration::from_millis(50), 30, u64::MAX).is_err());
        let mut cadence = Cadence::new(Duration::from_millis(50), 30, 0).unwrap();
        assert_eq!(cadence.after_turn(u64::MAX), Err(Error::Closed));
        assert_eq!(cadence.wake(u64::MAX, None), Err(Error::Closed));
    }
}
