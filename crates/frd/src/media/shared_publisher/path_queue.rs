//! Whether a viewer's path is holding a queue, from QUIC's own estimates.
//!
//! On a link slower than the encoder's rate (a hotel's, fr-7al9), QUIC never
//! refuses a datagram: nothing is lost while the bottleneck buffers the excess,
//! so the congestion window keeps growing. Pictures then queue at the
//! bottleneck until a reliable record, or the ACK for one, waits behind them
//! for longer than its delivery deadline and the session ends with Expired.
//! Namespace runs at 1 Mbit/s against a 2 Mbit/s encoder held 280 KB in flight
//! (over 2 s of queue) before the viewer's records expired.
//!
//! The shared publisher skips a capture while every healthy viewer's path is
//! queued, and a controlled stream while its one viewer's path is, so a narrow
//! link costs frames instead of the session. Skipping a capture keeps the
//! reference chain: the next picture still follows the last one sent.
//!
//! A round trip shows a queue only once a packet that waited in it is
//! acknowledged, and its ACK may wait in the same queue: pictures captured
//! meanwhile reached 144 KB in flight (over 1 s at 1 Mbit/s) before the first
//! late sample. So a path is also queued as soon as its bytes in flight would
//! take longer than the limit to drain at its measured delivery rate. ACKs
//! that waited arrive in bursts (a 50 ms interval measured ten times a 1 Mbit/s
//! link), so the rate is what was delivered over the busy time of the last one
//! to two seconds, each packet counted once whenever its ACK arrived.
//!
//! A longer route (a relay instead of a direct path) also raises the round
//! trip, but it does not drain. So once a path drains, the lowest round trip
//! over the next two is its route: the first packets sent into an empty queue
//! come back near the old baseline unless the route itself grew.

use fr_transport::quic::PathLoad;

/// Queueing delay a path may hold before capture waits for it to drain:
/// above what a healthy link shows (a 5K IDR is 48 ms at 50 Mbit/s), and well
/// below the 1 s minimum delivery allowance the queue would otherwise reach.
pub(super) const QUEUE_LIMIT_US: u64 = 150_000;
/// In-flight bytes are judged against the delivery rate only once the round
/// trip has risen this much: a fast link drains a burst (an IDR) before any
/// sample rises, whatever an application-limited rate measured.
pub(super) const RISE_US: u64 = QUEUE_LIMIT_US / 3;
/// At most this much in flight is no queue, whatever the last sample said:
/// the sample may be from a packet that waited behind pictures already acked.
pub(super) const DRAINED_BYTES: u64 = 4 * 1200;
/// A path whose round trip never came within the limit of its lowest sample
/// for a whole window of this length has a longer route now (a relay), not a
/// queue. The drain probe normally finds the route first; this window covers
/// a path that never drains.
pub(super) const BASELINE_WINDOW_US: u64 = 5_000_000;
/// Bytes counted per acknowledged packet. Video datagrams fill their packets,
/// and they are what a congested path carries.
pub(super) const PACKET_BYTES: u64 = 1200;
/// The delivery rate covers the busy time of the last one to two windows of
/// this length. Time after an idle sample measured the sender, not the path.
pub(super) const RATE_WINDOW_US: u64 = 1_000_000;
/// Less busy time than this measures no rate.
pub(super) const MIN_BUSY_US: u64 = 100_000;

/// Busy time (microseconds) and the packets delivered in it.
type Delivered = (u64, u64);

#[derive(Debug, Default)]
pub(crate) struct PathQueue {
    /// The lowest round trip seen: the path's own delay, without a queue.
    base_us: Option<u64>,
    /// When the current baseline window started, and its lowest sample.
    window: Option<(u64, u64)>,
    /// Since the path last drained: when the probe ends, and its lowest sample.
    probe: Option<(u64, u64)>,
    /// The previous sample: when, packets acknowledged by then, and whether
    /// the path was busy.
    last: Option<(u64, u64, bool)>,
    /// When the current rate window started, and the current and previous
    /// windows' busy time and packets delivered in it.
    delivery: Option<(u64, Delivered, Delivered)>,
    queued: bool,
}
impl PathQueue {
    pub(crate) const fn queued(&self) -> bool {
        self.queued
    }
    /// What the path delivered while busy, in bits per second (0 unmeasured).
    /// Only a queued path's delivery measures the path rather than the sender.
    pub(crate) fn delivered_bits_per_second(&self) -> u64 {
        self.rate().saturating_mul(8)
    }
    /// Take one sample of the viewer's connection at host time `now`.
    pub(crate) fn observe(&mut self, load: Option<PathLoad>, now: u64) {
        let Some(load) = load else {
            self.queued = false;
            return;
        };
        let rtt = load.latest_rtt_us;
        let busy = load.bytes_in_flight > DRAINED_BYTES;
        let drained = !busy && self.last.is_some_and(|(_, _, was_busy)| was_busy);
        let base = self.baseline(rtt, now);
        let base = self.adopt_route(drained, base, rtt, now);
        self.measure(load, busy, now);
        let rate = self.rate();
        let limit = base.saturating_add(QUEUE_LIMIT_US);
        // Microseconds to drain what is in flight at the measured rate.
        let standing = rate > 0
            && rtt > base.saturating_add(RISE_US)
            && load.bytes_in_flight.saturating_mul(1_000_000) / rate > limit;
        self.queued = busy && (rtt > limit || standing);
    }
    fn baseline(&mut self, rtt: u64, now: u64) -> u64 {
        let mut base = self.base_us.map_or(rtt, |base| base.min(rtt));
        let (started, lowest) = self
            .window
            .map_or((now, rtt), |(started, lowest)| (started, lowest.min(rtt)));
        if now.saturating_sub(started) >= BASELINE_WINDOW_US {
            if lowest > base.saturating_add(QUEUE_LIMIT_US) {
                base = lowest;
            }
            self.window = Some((now, rtt));
        } else {
            self.window = Some((started, lowest));
        }
        self.base_us = Some(base);
        base
    }
    /// Adopt a longer route once the round trips just after a drain were all
    /// past the limit; returns the baseline to judge this sample by.
    fn adopt_route(&mut self, drained: bool, base: u64, rtt: u64, now: u64) -> u64 {
        if drained && self.probe.is_none() {
            self.probe = Some((now.saturating_add(rtt.saturating_mul(2)), rtt));
        }
        let Some((until, lowest)) = self.probe else {
            return base;
        };
        let lowest = lowest.min(rtt);
        if now < until {
            self.probe = Some((until, lowest));
            return base;
        }
        self.probe = None;
        if lowest <= base.saturating_add(QUEUE_LIMIT_US) {
            return base;
        }
        self.base_us = Some(lowest);
        self.window = Some((now, rtt));
        lowest
    }
    fn measure(&mut self, load: PathLoad, busy: bool, now: u64) {
        self.rotate(now);
        if let Some((at, acked, was_busy)) = self.last
            && was_busy
            && let Some(delivered) = load.packets_acked.checked_sub(acked)
        {
            let (_, current, _) = self.delivery.get_or_insert((now, (0, 0), (0, 0)));
            current.0 = current.0.saturating_add(now.saturating_sub(at));
            current.1 = current.1.saturating_add(delivered);
        }
        self.last = Some((now, load.packets_acked, busy));
    }
    /// Start the next rate window once the current one is over.
    fn rotate(&mut self, now: u64) {
        if let Some((started, current, _)) = self.delivery {
            let elapsed = now.saturating_sub(started);
            if elapsed >= 2 * RATE_WINDOW_US {
                self.delivery = Some((now, (0, 0), (0, 0)));
            } else if elapsed >= RATE_WINDOW_US {
                self.delivery = Some((now, (0, 0), current));
            }
        }
    }
    /// Bytes per second delivered over the busy time of both windows.
    fn rate(&self) -> u64 {
        let Some((_, (busy, packets), (earlier_busy, earlier_packets))) = self.delivery else {
            return 0;
        };
        let busy = busy.saturating_add(earlier_busy);
        if busy < MIN_BUSY_US {
            return 0;
        }
        packets
            .saturating_add(earlier_packets)
            .saturating_mul(PACKET_BYTES)
            .saturating_mul(1_000_000)
            / busy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: u64 = DRAINED_BYTES + 1;

    fn load(latest_rtt_us: u64, bytes_in_flight: u64) -> PathLoad {
        acked(latest_rtt_us, bytes_in_flight, 0)
    }

    fn acked(latest_rtt_us: u64, bytes_in_flight: u64, packets_acked: u64) -> PathLoad {
        PathLoad {
            latest_rtt_us,
            bytes_in_flight,
            packets_acked,
        }
    }

    fn rate_at(path: &mut PathQueue, now: u64) -> u64 {
        path.rotate(now);
        path.rate()
    }

    #[test]
    fn a_path_is_queued_only_past_the_limit_above_its_lowest_round_trip() {
        let mut path = PathQueue::default();
        path.observe(None, 0);
        assert!(!path.queued(), "no sample is no queue");
        path.observe(Some(load(30_000, FULL)), 1_000);
        path.observe(Some(load(10_000, FULL)), 2_000);
        assert!(!path.queued());
        // The lowest sample is the baseline, not the first one.
        path.observe(Some(load(10_000 + QUEUE_LIMIT_US, FULL)), 3_000);
        assert!(!path.queued(), "at the limit is not past it");
        path.observe(Some(load(10_001 + QUEUE_LIMIT_US, FULL)), 4_000);
        assert!(path.queued());
        path.observe(Some(load(10_001 + QUEUE_LIMIT_US, DRAINED_BYTES)), 5_000);
        assert!(!path.queued(), "a drained path holds no queue");
        path.observe(Some(load(400_000, FULL)), 6_000);
        assert!(path.queued());
        path.observe(None, 7_000);
        assert!(!path.queued(), "a closed connection holds no queue");
    }

    #[test]
    fn a_longer_route_becomes_the_baseline_after_a_window_without_a_low_sample() {
        let mut path = PathQueue::default();
        path.observe(Some(load(5_000, FULL)), 0);
        // The route grew by 200 ms just after the first window opened.
        let relay = 205_000;
        path.observe(Some(load(relay, FULL)), 1);
        assert!(path.queued());
        // The first window still holds the old route's sample.
        path.observe(Some(load(relay, FULL)), BASELINE_WINDOW_US);
        assert!(path.queued());
        path.observe(Some(load(relay, FULL)), 2 * BASELINE_WINDOW_US - 1);
        assert!(path.queued());
        path.observe(Some(load(relay, FULL)), 2 * BASELINE_WINDOW_US);
        assert!(!path.queued(), "a whole window without a low sample");
        path.observe(
            Some(load(relay + QUEUE_LIMIT_US + 1, FULL)),
            2 * BASELINE_WINDOW_US + 1,
        );
        assert!(path.queued(), "the new baseline still bounds the queue");
    }

    #[test]
    fn a_drained_sample_in_a_window_keeps_the_baseline() {
        let mut path = PathQueue::default();
        path.observe(Some(load(5_000, FULL)), 0);
        path.observe(Some(load(400_000, FULL)), 1);
        path.observe(Some(load(400_000, FULL)), BASELINE_WINDOW_US);
        assert!(path.queued());
        // Capture waited, the queue drained, and the path showed its delay.
        path.observe(Some(load(6_000, DRAINED_BYTES)), BASELINE_WINDOW_US * 3 / 2);
        assert!(!path.queued());
        path.observe(Some(load(400_000, FULL)), 2 * BASELINE_WINDOW_US);
        assert!(path.queued(), "the window's low sample kept the baseline");
        path.observe(Some(load(400_000, FULL)), 3 * BASELINE_WINDOW_US);
        assert!(!path.queued(), "the next window had none");
    }

    #[test]
    fn a_drained_path_that_stays_slow_has_a_longer_route() {
        let mut path = PathQueue::default();
        path.observe(Some(load(5_000, FULL)), 0);
        // The route grows by 200 ms; capture waits for the path to drain.
        path.observe(Some(load(205_000, FULL)), 10_000);
        assert!(path.queued());
        path.observe(Some(load(205_000, 0)), 20_000);
        assert!(!path.queued());
        path.observe(Some(load(206_000, FULL)), 200_000);
        assert!(
            path.queued(),
            "judged by the old route until the probe ends"
        );
        path.observe(Some(load(205_500, FULL)), 430_000);
        assert!(!path.queued(), "every sample after the drain was slow");
        path.observe(Some(load(205_500 + QUEUE_LIMIT_US + 1, FULL)), 440_000);
        assert!(path.queued(), "the new route still bounds the queue");
    }

    #[test]
    fn a_drained_path_that_answers_fast_had_a_queue() {
        let mut path = PathQueue::default();
        path.observe(Some(load(5_000, FULL)), 0);
        path.observe(Some(load(700_000, FULL)), 10_000);
        assert!(path.queued());
        path.observe(Some(load(700_000, 0)), 20_000);
        // The first packet sent into the drained queue.
        path.observe(Some(load(9_000, FULL)), 100_000);
        path.observe(Some(load(400_000, FULL)), 1_420_000);
        assert!(
            path.queued(),
            "a fast sample after the drain kept the baseline"
        );
    }

    /// A path measured delivering 10 packets per 100 ms: 120 KB/s.
    fn measured_at_120_kb_per_second() -> PathQueue {
        let mut path = PathQueue::default();
        path.observe(Some(acked(1_000, FULL, 0)), 0);
        path.observe(Some(acked(1_000, FULL, 10)), 100_000);
        assert_eq!(rate_at(&mut path, 100_000), 120_000);
        assert!(!path.queued());
        path
    }

    #[test]
    fn bytes_in_flight_beyond_the_limit_at_the_delivery_rate_are_a_queue() {
        // 24 KB drains in 200 ms at 120 KB/s, past 1 ms + 150 ms.
        let mut path = measured_at_120_kb_per_second();
        path.observe(Some(acked(1_000 + RISE_US + 1, 24_000, 11)), 110_000);
        assert!(path.queued(), "before any sample shows the 150 ms");
        // 18 KB drains in 150 ms: within the limit.
        path.observe(Some(acked(1_000 + RISE_US + 1, 18_000, 12)), 120_000);
        assert!(!path.queued());
        // A burst that has not raised the round trip is no queue yet: a fast
        // link drains it first, whatever an application-limited rate said.
        let mut path = measured_at_120_kb_per_second();
        path.observe(Some(acked(1_000 + RISE_US, 240_000, 11)), 110_000);
        assert!(!path.queued());
    }

    #[test]
    fn delivery_counts_busy_time_only_and_ages_out() {
        let mut path = PathQueue::default();
        path.observe(Some(acked(1_000, FULL, 0)), 0);
        path.observe(Some(acked(1_000, FULL, 5)), 50_000);
        assert_eq!(rate_at(&mut path, 50_000), 0, "too little busy time");
        path.observe(Some(acked(1_000, FULL, 10)), 100_000);
        assert_eq!(rate_at(&mut path, 100_000), 120_000);
        // A burst of late ACKs counts its packets, not a rate of its own.
        path.observe(Some(acked(1_000, FULL, 40)), 101_000);
        path.observe(Some(acked(1_000, 0, 41)), 400_000);
        assert_eq!(rate_at(&mut path, 400_000), 41 * 1200 * 1_000_000 / 400_000);
        // Idle time after a drained sample measured the sender: not counted.
        path.observe(Some(acked(1_000, FULL, 41)), 900_000);
        assert_eq!(rate_at(&mut path, 900_000), 123_000);
        // The first window's delivery survives one more window, then is gone.
        assert_eq!(rate_at(&mut path, 50_000 + RATE_WINDOW_US), 123_000);
        assert_eq!(rate_at(&mut path, 50_000 + 3 * RATE_WINDOW_US), 0);
    }
}
