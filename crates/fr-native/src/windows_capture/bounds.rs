//! Bounds at the Windows hardware encoder's native event/output boundary.
//! The event pump never waits for its consumer: overflow retires this exact
//! inbox, including events already queued. An expired encode never consumes a
//! queued event, and native output sizes are checked before allocation or copy.
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    time::Instant,
};

/// Fixed-size, payload-free events. The queue retains at most this many; it
/// never retains a COM event or a media sample from the foreign producer.
pub(crate) const EVENT_CAPACITY: usize = 32;
const MAX_INPUT_CREDITS: u32 = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Event {
    NeedInput,
    HaveOutput,
    Failed,
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Failure {
    Expired,
    Overflow,
    Codec,
    Closed,
}

pub(crate) struct Sender {
    events: SyncSender<Event>,
    overflow: Arc<AtomicBool>,
}

pub(crate) struct Inbox {
    events: Receiver<Event>,
    overflow: Arc<AtomicBool>,
}

pub(crate) fn channel() -> (Sender, Inbox) {
    let (send, receive) = mpsc::sync_channel(EVENT_CAPACITY);
    let overflow = Arc::new(AtomicBool::new(false));
    (
        Sender {
            events: send,
            overflow: overflow.clone(),
        },
        Inbox {
            events: receive,
            overflow,
        },
    )
}

impl Sender {
    /// False is terminal for the pump. A full inbox is never a reason to block
    /// its shutdown or drop one event and continue a broken encoder sequence.
    pub(crate) fn send(&self, event: Event) -> bool {
        if self.overflow.load(Ordering::Acquire) {
            return false;
        }
        match self.events.try_send(event) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.overflow.store(true, Ordering::Release);
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }
}

impl Inbox {
    pub(crate) fn check(&self, deadline: Instant) -> Result<(), Failure> {
        if self.overflow.load(Ordering::Acquire) {
            return Err(Failure::Overflow);
        }
        if Instant::now() >= deadline {
            return Err(Failure::Expired);
        }
        Ok(())
    }

    pub(crate) fn next(&self, deadline: Instant) -> Result<Event, Failure> {
        self.check(deadline)?;
        let event = self
            .events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()));
        // recv_timeout(0) may still return an already queued event. A queue
        // backlog must not extend the caller's original deadline, even if it
        // expired between the first check and this wait.
        self.check(deadline)?;
        match event {
            Ok(Event::Failed) => Err(Failure::Codec),
            Ok(Event::Ended) | Err(RecvTimeoutError::Disconnected) => Err(Failure::Closed),
            Ok(event) => Ok(event),
            Err(RecvTimeoutError::Timeout) => Err(Failure::Expired),
        }
    }
}

/// Credits are retained native announcements, not permission to submit more
/// than the capture owner's one in-flight picture. A producer cannot grow the
/// credit counter indefinitely by staying just ahead of the bounded inbox.
#[derive(Default)]
pub(crate) struct Credits(u32);

impl Credits {
    pub(crate) const fn available(&self) -> bool {
        self.0 != 0
    }

    pub(crate) fn grant(&mut self) -> Result<(), Failure> {
        if self.0 >= MAX_INPUT_CREDITS {
            return Err(Failure::Overflow);
        }
        self.0 += 1;
        Ok(())
    }

    pub(crate) fn consume(&mut self) -> Result<(), Failure> {
        self.0 = self.0.checked_sub(1).ok_or(Failure::Codec)?;
        Ok(())
    }
}

/// Media Foundation timestamps are in 100 ns units. Keep the capture clock
/// for rate control, but distinguish even successive captures in the same
/// microsecond. Saturating here would let two submissions share an identity.
pub(crate) fn next_sample_time(previous: i64, micros: u64) -> Option<i64> {
    let clock = i64::try_from(micros).ok()?.checked_mul(10)?;
    Some(clock.max(previous.checked_add(1)?))
}

/// Only a complete sample from the sole stream and the original input can
/// carry that capture's frame id and source-observation timestamp.
pub(crate) fn owns_unit(
    submitted: i64,
    returned: Option<i64>,
    stream_status: u32,
    process_status: u32,
) -> bool {
    returned == Some(submitted) && stream_status == 0 && process_status == 0
}

/// The selected protocol's access-unit byte ceiling, carried into the native
/// boundary before Media Foundation is asked to allocate any output buffer.
#[derive(Clone, Copy)]
pub(crate) struct OutputLimit(u32);

impl OutputLimit {
    pub(crate) fn new(maximum: u32) -> Option<Self> {
        (maximum != 0).then_some(Self(maximum))
    }

    pub(crate) fn buffer_capacity(self, required: u32) -> Option<u32> {
        if required > self.0 {
            return None;
        }
        // Preserve the existing 1 MiB starting allocation only when the
        // selected limit admits it; a smaller negotiated ceiling stays smaller.
        Some(required.max((1 << 20).min(self.0)))
    }

    pub(crate) fn length(self, length: u32) -> Option<usize> {
        if length == 0 || length > self.0 {
            return None;
        }
        usize::try_from(length).ok()
    }

    pub(crate) fn locked_length(self, length: u32, capacity: u32) -> Option<usize> {
        (length <= capacity).then(|| self.length(length)).flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn until() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    #[test]
    fn event_order_is_preserved_without_retaining_native_payloads() {
        let (sender, inbox) = channel();
        for event in [Event::NeedInput, Event::HaveOutput, Event::NeedInput] {
            assert!(sender.send(event));
        }
        for event in [Event::NeedInput, Event::HaveOutput, Event::NeedInput] {
            assert_eq!(inbox.next(until()), Ok(event));
        }
    }

    #[test]
    fn overflow_fences_already_queued_events_and_stays_terminal() {
        let (sender, inbox) = channel();
        for _ in 0..EVENT_CAPACITY {
            assert!(sender.send(Event::NeedInput));
        }
        assert!(!sender.send(Event::HaveOutput));
        assert_eq!(inbox.next(until()), Err(Failure::Overflow));
        assert!(!sender.send(Event::NeedInput));
        assert_eq!(inbox.check(until()), Err(Failure::Overflow));
    }

    #[test]
    fn a_full_inbox_never_blocks_the_producer_or_its_shutdown() {
        let (sender, inbox) = channel();
        let (finished, completion) = mpsc::sync_channel(1);
        let producer = std::thread::spawn(move || {
            for _ in 0..EVENT_CAPACITY {
                assert!(sender.send(Event::NeedInput));
            }
            assert!(!sender.send(Event::Ended));
            finished.send(()).unwrap();
        });
        let completed = completion.recv_timeout(Duration::from_secs(5));
        let overflow = inbox.check(until());
        // Also release a wrongly blocked sender on failure, so the planted
        // blocking-send regression reports an assertion rather than hanging.
        drop(inbox);
        producer.join().unwrap();
        assert!(completed.is_ok());
        assert_eq!(overflow, Err(Failure::Overflow));
    }

    #[test]
    fn an_expired_encode_cannot_consume_a_queued_event_storm() {
        let (sender, inbox) = channel();
        for _ in 0..EVENT_CAPACITY {
            assert!(sender.send(Event::NeedInput));
        }
        let expired = Instant::now();
        for _ in 0..EVENT_CAPACITY {
            assert_eq!(inbox.next(expired), Err(Failure::Expired));
        }
        assert_eq!(inbox.events.try_iter().count(), EVENT_CAPACITY);
    }

    #[test]
    fn an_empty_inbox_wait_keeps_the_original_deadline() {
        let (_sender, inbox) = channel();
        let deadline = Instant::now() + Duration::from_millis(1);
        assert_eq!(inbox.next(deadline), Err(Failure::Expired));
        assert!(Instant::now() >= deadline);
    }

    #[test]
    fn native_failures_and_disconnected_producers_do_not_look_like_progress() {
        let (sender, inbox) = channel();
        assert!(sender.send(Event::Failed));
        assert_eq!(inbox.next(until()), Err(Failure::Codec));
        assert!(sender.send(Event::Ended));
        assert_eq!(inbox.next(until()), Err(Failure::Closed));
        drop(sender);
        assert_eq!(inbox.next(until()), Err(Failure::Closed));
        let (sender, inbox) = channel();
        drop(inbox);
        assert!(!sender.send(Event::NeedInput));
    }

    #[test]
    fn consumed_events_cannot_accumulate_unbounded_input_credit() {
        let (sender, inbox) = channel();
        let mut credits = Credits::default();
        assert_eq!(credits.consume(), Err(Failure::Codec));
        assert!(!credits.available());
        for _ in 0..MAX_INPUT_CREDITS {
            assert!(sender.send(Event::NeedInput));
            assert_eq!(inbox.next(until()), Ok(Event::NeedInput));
            credits.grant().unwrap();
        }
        assert!(credits.available());
        assert_eq!(credits.grant(), Err(Failure::Overflow));
        // One actual submission spends exactly one credit; overflow did not
        // wrap, reset or otherwise create a replacement allowance.
        credits.consume().unwrap();
        credits.grant().unwrap();
        for _ in 0..MAX_INPUT_CREDITS {
            credits.consume().unwrap();
        }
        assert!(!credits.available());
    }

    #[test]
    fn captures_use_distinct_native_times_without_stretching_the_capture_clock() {
        let first = next_sample_time(0, 5).unwrap();
        let second = next_sample_time(first, 5).unwrap();
        assert_eq!((first, second), (50, 51));
        assert_eq!(next_sample_time(second, 6), Some(60));
        assert_eq!(next_sample_time(i64::MAX, 5), None);
        assert_eq!(next_sample_time(0, (i64::MAX as u64 / 10) + 1), None);
        assert_eq!(next_sample_time(0, u64::MAX), None);
    }

    #[test]
    fn queued_extra_output_cannot_become_a_new_capture_even_at_the_same_clock() {
        let first = next_sample_time(0, 10).unwrap();
        let second = next_sample_time(first, 10).unwrap();
        assert!(owns_unit(first, Some(first), 0, 0));
        assert!(!owns_unit(second, Some(first), 0, 0));
        assert!(!owns_unit(second, None, 0, 0));
        assert!(!owns_unit(second, Some(second + 1), 0, 0));
        // INCOMPLETE and a newly announced stream both break this
        // transform's single-stream, one-complete-unit contract.
        assert!(!owns_unit(second, Some(second), 1 << 24, 0));
        assert!(!owns_unit(second, Some(second), 0, 1 << 8));
        assert!(owns_unit(second, Some(second), 0, 0));
    }

    #[test]
    fn output_requirements_are_checked_before_native_buffer_allocation() {
        assert!(OutputLimit::new(0).is_none());
        let limit = OutputLimit::new(256 * 1024).unwrap();
        for required in [0, 1, 256 * 1024] {
            assert_eq!(limit.buffer_capacity(required), Some(256 * 1024));
        }
        for required in [256 * 1024 + 1, 1 << 20, u32::MAX] {
            assert_eq!(limit.buffer_capacity(required), None);
        }
        let larger = OutputLimit::new(4 << 20).unwrap();
        assert_eq!(larger.buffer_capacity(0), Some(1 << 20));
        assert_eq!(larger.buffer_capacity(2 << 20), Some(2 << 20));
    }

    #[test]
    fn output_lengths_are_bounded_before_coalescing_and_copying() {
        let limit = OutputLimit::new(1024).unwrap();
        for length in [0, 1025, u32::MAX] {
            assert_eq!(limit.length(length), None);
            assert_eq!(limit.locked_length(length, u32::MAX), None);
        }
        assert_eq!(limit.length(1024), Some(1024));
        assert_eq!(limit.locked_length(1024, 1023), None);
        assert_eq!(limit.locked_length(1024, 1024), Some(1024));
        assert_eq!(limit.locked_length(16, 1024), Some(16));
    }
}
