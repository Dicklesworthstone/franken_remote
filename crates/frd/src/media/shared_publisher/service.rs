//! Cadenced source service, independent of each subscriber's network task.
use super::{CaptureReport, Error, MediaError, ObservationControl, Publisher};
use asupersync::{time::sleep_until, types::Time};
use std::{
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
    time::Duration,
};

const MAINTENANCE_US: u64 = 10_000;

#[derive(Debug, Clone, Copy)]
struct Cadence {
    interval: u64,
    next: u64,
    last: u64,
    /// The turn at `last` found no capture credit; see `may_start_early`.
    retrying: bool,
    /// When the last capture completed; see `waits_for_all`.
    shared: Option<u64>,
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
        Ok(Self {
            interval,
            next,
            last: now,
            retrying: false,
            shared: None,
        })
    }
    fn change_interval(&mut self, interval: u64) -> Result<(), Error> {
        if interval != self.interval {
            self.next = self.last.checked_add(interval).ok_or(Error::Closed)?;
            self.interval = interval;
        }
        Ok(())
    }
    fn after_turn(&mut self, started: u64, completed: u64) -> Result<(), Error> {
        if started < self.next || completed < started {
            return Err(Error::Closed);
        }
        // Work consumes this turn's interval rather than being added to it.
        // Anchor to ACTUAL admission, not an older scheduled wake: lateness
        // must never permit two captures less than one interval apart.
        // Work that outlasts the interval is followed by the next capture as
        // soon as it completes. Missed slots are not owed, so there is no
        // catch-up burst, and the next capture starts no earlier than one
        // interval after this one did. Rounding up to the next whole slot
        // instead idled the source after any capture that outlasted the
        // interval: at 60 fps, 40 ms of 5K work got a 50 ms period.
        let earliest = started.checked_add(self.interval).ok_or(Error::Closed)?;
        // The following turn starts no earlier than this completion and must
        // still be able to compute its own earliest start.
        completed.checked_add(self.interval).ok_or(Error::Closed)?;
        self.next = earliest.max(completed);
        self.last = started;
        self.retrying = false;
        Ok(())
    }
    fn after_capture(&mut self, started: u64, completed: u64) -> Result<(), Error> {
        self.after_turn(started, completed)?;
        self.shared = Some(completed);
        Ok(())
    }
    /// A turn at `next` that found no capture credit schedules the next turn
    /// as a capture would. Until then, credit that returns may start the
    /// capture early: see `may_start_early`.
    fn after_backpressure(&mut self, started: u64, completed: u64) -> Result<(), Error> {
        self.after_turn(started, completed)?;
        self.retrying = true;
        Ok(())
    }
    /// Whether a capture may start at `now`, before `next`. Only after a turn
    /// that found no credit: a viewer still held the previous picture. That
    /// turn ran no earlier than one interval after the previous capture
    /// started, so a capture started now is no closer to it.
    ///
    /// Credit returns when each viewer's connection task has handed the
    /// previous picture's last records to its transport, usually a few
    /// milliseconds later. Waiting for `next`, a whole interval after the turn
    /// that found none, cost up to one more interval per picture.
    fn may_start_early(&self, now: u64) -> bool {
        self.retrying && self.last <= now && now < self.next
    }
    fn after_early_capture(&mut self, started: u64, completed: u64) -> Result<(), Error> {
        if !self.may_start_early(started) {
            return Err(Error::Closed);
        }
        self.next = started;
        self.after_capture(started, completed)
    }
    /// Whether a capture at `now` waits for every viewer's credit instead of
    /// refusing those without it as slow. A viewer is slow only if it still
    /// holds the previous picture one interval after that picture was shared.
    /// Turns used to fall on whole slots, which left it between none and one
    /// interval; a turn may now come as soon as a long capture completes.
    fn waits_for_all(&self, now: u64) -> bool {
        self.shared
            .is_some_and(|shared| now < shared.saturating_add(self.interval))
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
    /// Sleep until `until` (host microseconds), or until a viewer has admitted
    /// records since `seen` was read from `credit_epoch`, whichever is first.
    /// True in the second case.
    async fn wait_for_credit(&self, until: u64, seen: u64) -> Result<bool, Error> {
        let nanos = until.checked_mul(1000).ok_or(Error::Closed)?;
        let mut sleep = pin!(sleep_until(Time::from_nanos(nanos)));
        let result = poll_fn(|task| {
            let Ok(mut members) = self.members.lock() else {
                return Poll::Ready(Err(Error::Poisoned));
            };
            if members.credit_epoch != seen {
                return Poll::Ready(Ok(true));
            }
            // Registered under the lock that advances the epoch: no admission
            // after this check can be missed.
            members.credit_waiter = Some(task.waker().clone());
            drop(members);
            if sleep.as_mut().poll(task).is_ready() {
                return Poll::Ready(Ok(false));
            }
            Poll::Pending
        })
        .await;
        if let Ok(mut members) = self.members.lock() {
            members.credit_waiter = None;
        }
        result
    }
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
        let already_served = self.served;
        self.served = true;
        let operation = ServiceOperation { publisher: self };
        async move {
            let publisher = &mut *operation.publisher;
            if already_served {
                return Err(Error::InvalidBudget);
            }
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
            // Cursor sampling retains the operator's original cadence while
            // verified-idle video admission slows down. Cursor motion never
            // fabricates a source observation or forces a video encode.
            let mut cursor_cadence = if let Some(pacing) = publisher.pacing() {
                if pacing.policy().minimum_interval_us != cadence.interval {
                    return Err(Error::InvalidBudget);
                }
                let cursor = cadence;
                cadence.change_interval(pacing.interval_us())?;
                Some(cursor)
            } else {
                None
            };
            loop {
                let (now, deadline, credit_epoch) = {
                    let mut members = publisher.members.lock().map_err(|_| Error::Poisoned)?;
                    members.tick()?;
                    (members.last, members.media_deadline(), members.credit_epoch)
                };
                if let Some(interval) = publisher.capture_interval(now)? {
                    cadence.change_interval(interval)?;
                }
                if now < cadence.next {
                    if cursor_cadence.is_some_and(|cursor| now >= cursor.next) {
                        publisher.sample_cursor().await?;
                        cursor_cadence
                            .as_mut()
                            .expect("checked")
                            .after_turn(now, owner.check().map_err(Error::Media)?.as_micros())?;
                        continue;
                    }
                    let wake = cadence
                        .wake(now, deadline)?
                        .min(cursor_cadence.map_or(u64::MAX, |cursor| cursor.next));
                    if !cadence.may_start_early(now) {
                        let nanos = wake.checked_mul(1000).ok_or(Error::Closed)?;
                        // A fresh Sleep per wait, never repoll a completed timer.
                        sleep_until(Time::from_nanos(nanos)).await;
                        continue;
                    }
                    if !publisher.wait_for_credit(wake, credit_epoch).await? {
                        continue;
                    }
                    let started = owner.check().map_err(Error::Media)?.as_micros();
                    if cadence.may_start_early(started) {
                        let wait_for_all = cadence.waits_for_all(started);
                        match publisher.capture_inner(wait_for_all).await {
                            Ok(update) => report(update),
                            Err(Error::Media(MediaError::Backpressure)) => continue,
                            Err(error) => return Err(error),
                        }
                        publisher
                            .sample_cursor_after(&owner, &mut cursor_cadence)
                            .await?;
                        let completed = owner.check().map_err(Error::Media)?.as_micros();
                        cadence.after_early_capture(started, completed)?;
                    }
                    continue;
                }
                let captured = match publisher.capture_inner(cadence.waits_for_all(now)).await {
                    Ok(update) => {
                        report(update);
                        true
                    }
                    Err(Error::Media(MediaError::Backpressure)) => false,
                    Err(error) => return Err(error),
                };
                publisher
                    .sample_cursor_after(&owner, &mut cursor_cadence)
                    .await?;
                let completed = owner.check().map_err(Error::Media)?.as_micros();
                if captured {
                    cadence.after_capture(now, completed)?;
                } else {
                    cadence.after_backpressure(now, completed)?;
                }
            }
        }
    }
    /// The cursor sample after each video turn, or on the cursor's own
    /// cadence while adaptive pacing slows video.
    async fn sample_cursor_after(
        &mut self,
        owner: &ObservationControl,
        cursor_cadence: &mut Option<Cadence>,
    ) -> Result<(), Error> {
        let started = owner.check().map_err(Error::Media)?.as_micros();
        if cursor_cadence.is_none_or(|cursor| started >= cursor.next) {
            self.sample_cursor().await?;
            if let Some(cursor) = cursor_cadence {
                cursor.after_turn(started, owner.check().map_err(Error::Media)?.as_micros())?;
            }
        }
        Ok(())
    }
}
impl super::Members {
    /// The earliest media deadline of a viewer still being served.
    fn media_deadline(&self) -> Option<u64> {
        self.entries
            .iter()
            .flatten()
            .filter(|e| e.failure.is_none())
            .filter_map(super::Entry::deadline)
            .min()
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
        cadence.after_turn(50_000, 900_000).unwrap();
        // One capture may follow at once; the 16 missed slots are not owed.
        assert_eq!(cadence.next, 900_000);
        cadence.after_turn(900_000, 901_000).unwrap();
        assert_eq!(cadence.next, 950_000);
        cadence.after_turn(950_000, 1_950_000).unwrap();
        assert_eq!(cadence.next, 1_950_000);
    }
    #[test]
    fn work_longer_than_the_interval_sets_the_rate_without_idling_to_a_slot() {
        // 60 fps: 5K encode-bound work of 40 ms runs at 25 pictures/s, and
        // 17 ms of work at ~59, not at the next whole multiple of the interval
        // (20 and 30 pictures/s). No start is ever less than an interval
        // after the previous one.
        for work in [17_000_u64, 40_000] {
            let mut cadence = Cadence::new(Duration::from_micros(16_667), 60, 0).unwrap();
            let mut previous = None;
            for _ in 0..1000 {
                let started = cadence.next;
                if let Some(previous) = previous {
                    assert_eq!(started - previous, work);
                    assert!(started - previous >= 16_667);
                }
                cadence.after_turn(started, started + work).unwrap();
                assert_eq!(cadence.next, started + work);
                previous = Some(started);
            }
        }
    }
    #[test]
    fn returned_credit_starts_the_capture_early_but_never_closer_than_an_interval() {
        let mut cadence = Cadence::new(Duration::from_micros(16_667), 60, 0).unwrap();
        cadence.after_turn(16_667, 30_000).unwrap();
        assert_eq!(cadence.next, 33_334);
        // After a capture, the next one waits a whole interval from its start.
        assert!(!cadence.may_start_early(30_000));
        assert_eq!(
            cadence.after_early_capture(30_000, 31_000),
            Err(Error::Closed)
        );
        // The turn at 33_334 finds no credit. Its successor is still a whole
        // interval later, but credit returning at 36_000 starts the capture.
        cadence.after_backpressure(33_334, 33_400).unwrap();
        assert_eq!(cadence.next, 50_001);
        assert!(!cadence.may_start_early(33_333));
        assert!(cadence.may_start_early(36_000));
        assert!(!cadence.may_start_early(50_001));
        // A 34 ms 5K capture: the next turn follows its completion.
        cadence.after_early_capture(36_000, 70_000).unwrap();
        assert_eq!((cadence.next, cadence.last), (70_000, 36_000));
        assert!(!cadence.may_start_early(60_000));
        assert_eq!(
            cadence.after_early_capture(60_000, 61_000),
            Err(Error::Closed)
        );
        // Each early start follows a turn without credit, which itself ran
        // at least one interval after the previous capture started.
        cadence.after_backpressure(70_000, 70_100).unwrap();
        cadence.after_early_capture(70_001, 80_000).unwrap();
        assert!(70_001 - 36_000 >= cadence.interval);
        assert_eq!(cadence.next, 86_668);
    }
    #[test]
    fn a_viewer_has_a_whole_interval_to_take_each_picture_before_it_is_slow() {
        // 60 fps and 37 ms of 5K work: the next turn comes as the capture
        // completes, before any viewer can have taken the picture.
        let mut cadence = Cadence::new(Duration::from_micros(16_667), 60, 0).unwrap();
        assert!(!cadence.waits_for_all(16_667));
        cadence.after_capture(16_667, 53_667).unwrap();
        assert_eq!(cadence.next, 53_667);
        assert!(cadence.waits_for_all(53_667));
        assert!(cadence.waits_for_all(70_333));
        assert!(!cadence.waits_for_all(70_334));
        // A turn without credit keeps the schedule; the turn after it is
        // past the picture's interval and may refuse a viewer still sending it.
        cadence.after_backpressure(53_700, 53_800).unwrap();
        assert_eq!(cadence.next, 70_367);
        assert!(!cadence.waits_for_all(cadence.next));
        // An early capture starts a new interval for its own picture.
        cadence.after_early_capture(60_000, 97_000).unwrap();
        assert!(cadence.waits_for_all(113_666));
        assert!(!cadence.waits_for_all(113_667));
    }
    #[test]
    fn a_backpressure_turn_keeps_the_schedule_and_refuses_one_before_its_time() {
        let mut cadence = Cadence::new(Duration::from_millis(50), 30, 0).unwrap();
        assert_eq!(
            cadence.after_backpressure(49_999, 50_000),
            Err(Error::Closed)
        );
        assert_eq!(
            cadence.after_backpressure(50_000, 49_999),
            Err(Error::Closed)
        );
        assert!(!cadence.retrying);
        cadence.after_backpressure(50_000, 50_500).unwrap();
        assert_eq!(cadence.next, 100_000);
        // No early start once the schedule's own turn is due; that turn
        // clears the retry whether it captures or not.
        cadence.after_turn(100_000, 101_000).unwrap();
        assert!(!cadence.retrying);
        assert_eq!(
            cadence.after_early_capture(100_500, 101_000),
            Err(Error::Closed)
        );
    }
    #[test]
    fn healthy_native_work_consumes_the_interval_instead_of_halving_the_rate() {
        for fps in [1, 15, 30, 60, 120, 240] {
            let interval = 1_000_000_u64.div_ceil(u64::from(fps));
            let mut cadence = Cadence::new(Duration::from_micros(interval), fps, 0).unwrap();
            for turn in 1..=500 {
                let started = cadence.next;
                assert_eq!(started, turn * interval);
                cadence.after_turn(started, started + interval - 1).unwrap();
                assert_eq!(cadence.next, (turn + 1) * interval);
            }
        }
    }
    #[test]
    fn late_wakes_reanchor_to_actual_admission_not_the_missed_schedule() {
        let mut cadence = Cadence::new(Duration::from_millis(50), 30, 0).unwrap();
        cadence.after_turn(99_999, 100_001).unwrap();
        assert_eq!(cadence.next, 149_999);
        assert_eq!(cadence.wake(100_001, None).unwrap(), 110_001);
    }
    #[test]
    fn adaptive_video_neither_slows_cursor_sampling_nor_replays_missed_slots() {
        let mut video = Cadence::new(Duration::from_millis(50), 30, 0).unwrap();
        let mut cursor = video;
        video.change_interval(200_000).unwrap();
        for started in [50_000, 100_000, 150_000] {
            assert_eq!(cursor.next, started);
            cursor.after_turn(started, started + 1000).unwrap();
            assert_eq!(video.next, 200_000);
        }
        video.after_turn(200_000, 450_000).unwrap();
        assert_eq!(video.next, 450_000);
        video.change_interval(100_000).unwrap();
        assert_eq!(video.next, 300_000);
        // Resume at actual current time, never replay the two elapsed slots.
        video.after_turn(450_000, 451_000).unwrap();
        assert_eq!(video.next, 550_000);
    }
    #[test]
    fn exact_expiry_and_long_native_stalls_owe_no_elapsed_slots() {
        let mut cadence = Cadence::new(Duration::from_millis(50), 30, 0).unwrap();
        cadence.after_turn(50_000, 100_000).unwrap();
        assert_eq!(cadence.next, 100_000);
        cadence.after_turn(150_000, 1_000_000_000_001).unwrap();
        assert_eq!(cadence.next, 1_000_000_000_001);
        cadence
            .after_turn(1_000_000_000_001, 1_000_000_000_002)
            .unwrap();
        assert_eq!(cadence.next, 1_000_000_050_001);
    }
    #[test]
    fn native_or_admission_clock_regression_never_rearms_capture() {
        let mut cadence = Cadence::new(Duration::from_millis(50), 30, 0).unwrap();
        for (started, completed) in [(49_999, 70_000), (50_000, 49_999)] {
            assert_eq!(cadence.after_turn(started, completed), Err(Error::Closed));
            assert_eq!(cadence.next, 50_000);
        }
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
        assert_eq!(cadence.after_turn(50_000, u64::MAX), Err(Error::Closed));
        assert_eq!(cadence.next, 50_000);
        assert_eq!(cadence.wake(u64::MAX, None), Err(Error::Closed));
    }
}
