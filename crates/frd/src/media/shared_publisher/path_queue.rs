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
//! Until a delayed ACK arrives, the RTT still describes the old flight. Hold
//! new dependent captures when a continuously sampled busy flight has made no
//! ACK progress for two round trips plus four variations (at least the queue
//! allowance). This alone cannot prove delivered throughput or reduce bitrate.
//!
//! A longer route (a relay instead of a direct path) also raises the round
//! trip, but it does not drain. So once a path drains, the lowest round trip
//! over the next two is its route: the first packets sent into an empty queue
//! come back near the old baseline unless the route itself grew.

use fr_media::bitrate::PathEvidence;
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
pub(super) const DRAINED_BYTES: u64 = fr_transport::quic::BUSY_PATH_BYTES;
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
/// Acknowledged busy traffic or a losslessly drained video flight can justify
/// a short, bounded headroom observation between service turns. An idle
/// connection cannot keep that evidence alive by rereading transport counters.
const HEADROOM_EVIDENCE_US: u64 = 250_000;
/// Shared viewers can be serviced independently. Consuming a member's cached
/// path must not renew its sampling lifetime when that viewer has stopped.
const SAMPLE_EVIDENCE_US: u64 = 100_000;

/// Busy time (microseconds) and the packets delivered in it.
type Delivered = (u64, u64);

/// A drain arms a probe; only later, genuinely new round trips can measure it.
/// The RTT on the drain itself may still belong to a packet from the old queue.
#[derive(Debug, Default)]
struct RouteProbe {
    sample: Option<(u64, u64)>,
    until: u64,
    lowest: u64,
}

#[derive(Debug, Default)]
pub(crate) struct PathQueue {
    /// The lowest round trip seen: the path's own delay, without a queue.
    base_us: Option<u64>,
    /// When the current baseline window started, and its lowest sample.
    window: Option<(u64, u64)>,
    /// Fresh round trips observed after the path last drained.
    probe: Option<RouteProbe>,
    /// The previous sample: when, packets acknowledged by then, and whether
    /// the path was busy.
    last: Option<(u64, u64, bool)>,
    /// Original allowance for the current continuously busy, unacknowledged flight.
    ack_deadline: Option<u64>,
    /// The last estimator snapshot. A new packet ACK is not necessarily a new
    /// RTT sample (reordered ACK ranges can acknowledge only older packets).
    rtt: Option<(u64, u64, u64)>,
    /// Last positive RTT observation. Repeated reads cannot fill a route window.
    rtt_at: Option<u64>,
    /// When the current rate window started, and the current and previous
    /// windows' busy time and packets delivered in it.
    delivery: Option<(u64, Delivered, Delivered)>,
    /// Most recent acknowledged progress from a previously busy path.
    clear_at: Option<u64>,
    /// Loss can occur and ACKs can resume between two reads of the path.
    /// Retain the counter and the time we first saw its latest change.
    loss_count: Option<u64>,
    loss_observed_at: Option<u64>,
    /// Consume each native busy-flight progress witness once. Its original
    /// timestamp, rather than receipt here, bounds its use as headroom.
    busy_packets_acked: u64,
    queued: bool,
}
impl PathQueue {
    pub(crate) const fn queued(&self) -> bool {
        self.queued
    }
    /// Suppress ordinary capture until ACK progress or drain. Stale member
    /// samples cannot keep suppressing a shared publisher after service stops.
    pub(crate) fn waiting_for_ack(&self, now: u64) -> bool {
        self.last
            .zip(self.ack_deadline)
            .is_some_and(|((at, _, _), until)| {
                at >= until
                    && now
                        .checked_sub(at)
                        .is_some_and(|age| age <= SAMPLE_EVIDENCE_US)
            })
    }
    /// What the path delivered while busy, in bits per second (0 unmeasured).
    /// Only a queued path's delivery measures the path rather than the sender.
    pub(crate) fn delivered_bits_per_second(&self) -> u64 {
        self.rate().saturating_mul(8)
    }
    /// Advisory evidence for the rate controller. A clear queue alone is not
    /// measured headroom: after traffic stops, the last RTT and delivery rate
    /// remain readable but no longer justify an optimistic encoder change.
    pub(crate) fn evidence(&self, now: u64) -> PathEvidence {
        let Some((sampled, _, _)) = self.last else {
            return PathEvidence::Unknown;
        };
        let Some(age) = now.checked_sub(sampled) else {
            return PathEvidence::Unknown;
        };
        if age > SAMPLE_EVIDENCE_US {
            return PathEvidence::Unknown;
        }
        if self.queued {
            PathEvidence::Queued {
                delivered_bits_per_second: self.delivered_bits_per_second(),
            }
        } else if self.waiting_for_ack(now) {
            PathEvidence::Unknown
        } else if self.clear_at.is_some_and(|at| {
            now.checked_sub(at)
                .is_some_and(|age| age <= HEADROOM_EVIDENCE_US)
        }) {
            PathEvidence::Clear
        } else {
            PathEvidence::Idle
        }
    }
    /// Take one sample of the viewer's connection at host time `now`.
    pub(crate) fn observe(&mut self, load: Option<PathLoad>, now: u64) {
        let Some(load) = load else {
            // No connection/sample cannot preserve another path's baseline,
            // pending drain probe, or delivery measurement.
            *self = Self::default();
            return;
        };
        if self
            .last
            .is_some_and(|(at, acked, _)| now < at || load.packets_acked < acked)
        {
            // A clock or counter discontinuity is not path progress. Reset
            // advisory history without changing any session/authority deadline.
            *self = Self::default();
        } else if self
            .last
            .is_some_and(|(at, _, _)| now - at > RATE_WINDOW_US)
        {
            // A long service gap cannot measure busy time or a route change.
            // Retain the known baseline conservatively; collect new evidence
            // before adopting a longer route or measuring delivery again.
            self.last = None;
            self.delivery = None;
            self.clear_at = None;
            self.window = None;
            self.probe = None;
        }
        let rtt = load.latest_rtt_us;
        let busy = load.bytes_in_flight > DRAINED_BYTES;
        let drained = !busy && self.last.is_some_and(|(_, _, was_busy)| was_busy);
        let progressed = self
            .last
            .is_some_and(|(_, acked, _)| load.packets_acked > acked);
        if !busy {
            self.ack_deadline = None;
        } else if progressed
            || self
                .last
                .is_none_or(|(at, _, was_busy)| !was_busy || now - at > SAMPLE_EVIDENCE_US)
        {
            // Start on the first observed busy flight, including after a gap.
            // Neither idle time nor time before a later ACK consumes its budget.
            let allowance = rtt
                .max(load.smoothed_rtt_us)
                .saturating_mul(2)
                .saturating_add(load.rtt_variation_us.saturating_mul(4))
                .max(QUEUE_LIMIT_US);
            self.ack_deadline = now.checked_add(allowance);
        }
        let estimator = (rtt, load.smoothed_rtt_us, load.rtt_variation_us);
        let sampled_rtt = progressed && self.rtt != Some(estimator);
        let rtt_gap = self.rtt_at.zip(self.rtt).is_some_and(|(at, previous)| {
            now - at > previous.0.saturating_mul(2).max(SAMPLE_EVIDENCE_US)
        });
        self.rtt = Some(estimator);
        let base = if let (Some(base), false) = (self.base_us, sampled_rtt) {
            base
        } else {
            if rtt_gap {
                // An occasional new ACK cannot complete a five-second route
                // window made mostly of stale readings. Keep the known route
                // and start this observation window with new evidence.
                self.window = None;
            }
            self.rtt_at = Some(now);
            self.baseline(rtt, now)
        };
        // A changed estimator plus ACK progress is positive RTT evidence.
        // Repeated, fully converged estimates remain usable for queue bounds
        // and delivery, but cannot certify a longer route by themselves.
        let base = self.adopt_route(drained && progressed, sampled_rtt, base, rtt, now);
        if self
            .loss_count
            .is_some_and(|last| load.packets_lost != last)
        {
            self.loss_observed_at = Some(now);
        }
        self.loss_count = Some(load.packets_lost);
        // Even a later busy ACK cannot hide a loss between samples. Keep
        // the veto observable across a shared publisher capture interval.
        // It never suppresses measured queue pressure or fabricates a cut.
        if load.packets_lost == u64::MAX
            || self
                .loss_observed_at
                .is_some_and(|at| now - at <= HEADROOM_EVIDENCE_US)
        {
            self.clear_at = None;
        } else if load.busy_packets_acked > self.busy_packets_acked
            && load.busy_packets_acked <= load.packets_acked
            && let Some(at) = load.busy_acked_at_us
            && now
                .checked_sub(at)
                .is_some_and(|age| age <= HEADROOM_EVIDENCE_US)
        {
            self.clear_at = Some(at);
        }
        self.busy_packets_acked = load.busy_packets_acked;
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
    fn adopt_route(&mut self, drained: bool, sampled: bool, base: u64, rtt: u64, now: u64) -> u64 {
        if sampled && rtt <= base.saturating_add(QUEUE_LIMIT_US) {
            // A genuinely fast answer, including the one that drained the
            // flight, disproves a longer route. A later congested burst must
            // not discard that answer by starting its own post-drain window.
            self.probe = None;
            return base;
        }
        if drained && self.probe.is_none() {
            self.probe = Some(RouteProbe::default());
            return base;
        }
        if !sampled {
            return base;
        }
        let Some(probe) = &mut self.probe else {
            return base;
        };
        if probe.sample.is_none_or(|(at, previous_rtt)| {
            now - at > previous_rtt.saturating_mul(2).max(SAMPLE_EVIDENCE_US)
        }) {
            // Neither an old drain nor an idle interval can mature a route
            // probe. Begin again with this first fresh answer, and require
            // subsequent answers throughout the next two round trips.
            probe.until = now.saturating_add(rtt.saturating_mul(2));
            probe.lowest = rtt;
            probe.sample = Some((now, rtt));
            return base;
        }
        probe.lowest = probe.lowest.min(rtt);
        probe.sample = Some((now, rtt));
        if now < probe.until {
            return base;
        }
        let lowest = probe.lowest;
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

    /// Each call represents a newly acknowledged packet, not another read of
    /// the same transport statistics.
    fn sample(path: &mut PathQueue, rtt: u64, bytes: u64, now: u64) {
        let packets = path.last.map_or(1, |(_, packets, _)| packets + 1);
        let mut load = acked(rtt, bytes, packets);
        // A changing smoothed estimator witnesses a new RTT sample even when
        // individual latest RTT values happen to be identical.
        load.smoothed_rtt_us = rtt + packets % 2;
        path.observe(Some(load), now);
    }

    fn acked(latest_rtt_us: u64, bytes_in_flight: u64, packets_acked: u64) -> PathLoad {
        PathLoad {
            latest_rtt_us,
            smoothed_rtt_us: latest_rtt_us,
            rtt_variation_us: latest_rtt_us / 2,
            bytes_in_flight,
            packets_acked,
            packets_lost: 0,
            busy_packets_acked: 0,
            busy_acked_at_us: None,
        }
    }

    fn busy_progress(mut load: PathLoad, at: u64) -> PathLoad {
        load.busy_packets_acked = load.packets_acked;
        load.busy_acked_at_us = Some(at);
        load
    }

    fn current(path: &PathQueue) -> PathEvidence {
        path.evidence(path.last.map_or(0, |(at, _, _)| at))
    }

    #[test]
    fn an_unacknowledged_busy_flight_holds_capture_before_the_rtt_rises() {
        let mut path = PathQueue::default();
        let mut load = busy_progress(acked(5_000, FULL, 17), 0);
        path.observe(Some(load), 0);
        assert_eq!(current(&path), PathEvidence::Clear);
        // The last RTT is still 5 ms while more pictures enter this flight.
        for at in [75_000, 149_999, 150_000, 200_000] {
            load.bytes_in_flight += 12_000;
            path.observe(Some(load), at);
            assert!(!path.queued(), "no measured standing queue yet");
            assert_eq!(path.waiting_for_ack(at), at >= 150_000);
        }
        assert_eq!(current(&path), PathEvidence::Unknown);
        // A single ACK releases only the silence hold. An actual RTT rise
        // still blocks capture and supplies the real queue evidence for a cut.
        load.packets_acked += 1;
        load.latest_rtt_us = 180_000;
        path.observe(Some(load), 210_000);
        assert!(!path.waiting_for_ack(210_000));
        assert!(path.queued());
        assert!(matches!(current(&path), PathEvidence::Queued { .. }));
    }

    #[test]
    fn a_normal_high_latency_flight_keeps_its_rtt_and_variance_allowance() {
        let mut path = PathQueue::default();
        let mut load = acked(110_000, FULL, 21);
        load.smoothed_rtt_us = 120_000;
        load.rtt_variation_us = 30_000;
        // 2 * 120 ms + 4 * 30 ms. A high-latency route keeps its capture
        // opportunity past the 150 ms floor even without intervening ACKs.
        for at in [0, 100_000, 200_000, 300_000, 359_999, 360_000] {
            path.observe(Some(load), at);
            assert_eq!(path.waiting_for_ack(at), at == 360_000);
        }
        load.packets_acked += 1;
        path.observe(Some(load), 361_000);
        assert!(!path.waiting_for_ack(361_000));
        assert!(!path.queued());
    }

    #[test]
    fn service_gaps_idle_flights_and_replaced_counters_cannot_age_an_ack_hold() {
        let mut path = PathQueue::default();
        let load = acked(1_000, FULL, 20);
        for at in [0, 100_000, 150_000] {
            path.observe(Some(load), at);
        }
        assert!(path.waiting_for_ack(150_000));
        assert!(!path.waiting_for_ack(250_001), "stale shared-member sample");
        path.observe(Some(load), 250_001);
        assert!(!path.waiting_for_ack(250_001), "service gap");
        path.observe(Some(acked(1_000, DRAINED_BYTES, 20)), 300_000);
        path.observe(Some(load), 350_000);
        path.observe(Some(load), 450_000);
        assert!(!path.waiting_for_ack(450_000), "idle time is not busy time");
        path.observe(Some(load), 500_000);
        assert!(path.waiting_for_ack(500_000));
        path.observe(Some(acked(1_000, FULL, 0)), 510_000);
        assert!(!path.waiting_for_ack(510_000), "new counter lifetime");
        path.observe(None, 520_000);
        assert_eq!(current(&path), PathEvidence::Unknown);
        assert!(!path.waiting_for_ack(520_000));
    }

    #[test]
    fn loss_veto_preserves_queue_evidence_and_needs_a_new_clean_witness() {
        let mut path = PathQueue::default();
        path.observe(Some(busy_progress(acked(5_000, FULL, 1), 0)), 0);
        assert_eq!(current(&path), PathEvidence::Clear);
        let mut load = busy_progress(acked(200_000, FULL, 2), 100);
        load.packets_lost = 1;
        path.observe(Some(load), 100);
        assert!(matches!(current(&path), PathEvidence::Queued { .. }));
        load.bytes_in_flight = 0;
        path.observe(Some(load), 200);
        assert_eq!(current(&path), PathEvidence::Idle);
        path.observe(Some(load), 300_000);
        assert_eq!(current(&path), PathEvidence::Idle, "old ACK cannot resume");
        load.packets_acked += 1;
        path.observe(Some(busy_progress(load, 300_001)), 300_001);
        assert_eq!(current(&path), PathEvidence::Clear);
        load.packets_lost = u64::MAX;
        for at in [300_002, 600_000] {
            load.packets_acked += 1;
            path.observe(Some(busy_progress(load, at)), at);
            assert_eq!(current(&path), PathEvidence::Idle);
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
        sample(&mut path, 30_000, FULL, 1_000);
        sample(&mut path, 10_000, FULL, 2_000);
        assert!(!path.queued());
        // The lowest sample is the baseline, not the first one.
        sample(&mut path, 10_000 + QUEUE_LIMIT_US, FULL, 3_000);
        assert!(!path.queued(), "at the limit is not past it");
        sample(&mut path, 10_001 + QUEUE_LIMIT_US, FULL, 4_000);
        assert!(path.queued());
        sample(&mut path, 10_001 + QUEUE_LIMIT_US, DRAINED_BYTES, 5_000);
        assert!(!path.queued(), "a drained path holds no queue");
        sample(&mut path, 400_000, FULL, 6_000);
        assert!(path.queued());
        path.observe(None, 7_000);
        assert!(!path.queued(), "a closed connection holds no queue");
    }

    #[test]
    fn a_longer_route_becomes_the_baseline_after_a_window_without_a_low_sample() {
        let mut path = PathQueue::default();
        sample(&mut path, 5_000, FULL, 0);
        // Regular, new ACK samples establish the route. Merely rereading the
        // first high RTT for two windows is not evidence of a longer route.
        let relay = 205_000;
        for now in (100_000..2 * BASELINE_WINDOW_US).step_by(100_000) {
            sample(&mut path, relay, FULL, now);
            assert!(path.queued());
        }
        sample(&mut path, relay, FULL, 2 * BASELINE_WINDOW_US);
        assert!(!path.queued(), "a whole window without a low sample");
        sample(
            &mut path,
            relay + QUEUE_LIMIT_US + 1,
            FULL,
            2 * BASELINE_WINDOW_US + 1,
        );
        assert!(path.queued(), "the new baseline still bounds the queue");
    }

    #[test]
    fn a_drained_sample_in_a_window_keeps_the_baseline() {
        let mut path = PathQueue::default();
        sample(&mut path, 5_000, FULL, 0);
        for now in (100_000..3 * BASELINE_WINDOW_US).step_by(100_000) {
            if now == BASELINE_WINDOW_US * 3 / 2 {
                // Capture waited, the queue drained, and the path showed its
                // real delay inside the second baseline window.
                sample(&mut path, 6_000, DRAINED_BYTES, now);
                assert!(!path.queued());
            } else {
                sample(&mut path, 400_000, FULL, now);
                assert!(path.queued(), "the observed low sample kept the baseline");
            }
        }
        sample(&mut path, 400_000, FULL, 3 * BASELINE_WINDOW_US);
        assert!(!path.queued(), "the next window had no low sample");
    }

    #[test]
    fn a_drained_path_that_stays_slow_has_a_longer_route() {
        let mut path = PathQueue::default();
        sample(&mut path, 5_000, FULL, 0);
        // The route grows by 200 ms; capture waits for the path to drain.
        sample(&mut path, 205_000, FULL, 10_000);
        assert!(path.queued());
        sample(&mut path, 205_000, 0, 20_000);
        assert!(!path.queued());
        sample(&mut path, 206_000, FULL, 200_000);
        assert!(
            path.queued(),
            "judged by the old route until the probe ends"
        );
        sample(&mut path, 205_500, FULL, 430_000);
        assert!(
            path.queued(),
            "two new round trips are still being observed"
        );
        sample(&mut path, 205_250, FULL, 650_000);
        assert!(!path.queued(), "every sample after the drain was slow");
        sample(&mut path, 205_250 + QUEUE_LIMIT_US + 1, FULL, 660_000);
        assert!(path.queued(), "the new route still bounds the queue");
    }

    #[test]
    fn a_drained_path_that_answers_fast_had_a_queue() {
        let mut path = PathQueue::default();
        sample(&mut path, 5_000, FULL, 0);
        sample(&mut path, 700_000, FULL, 10_000);
        assert!(path.queued());
        sample(&mut path, 700_000, 0, 20_000);
        // The first packet sent into the drained queue.
        sample(&mut path, 9_000, FULL, 100_000);
        sample(&mut path, 400_000, FULL, 1_420_000);
        assert!(
            path.queued(),
            "a fast sample after the drain kept the baseline"
        );
    }

    /// A path measured delivering 10 packets per 100 ms: 120 KB/s.
    fn measured_at_120_kb_per_second() -> PathQueue {
        let mut path = PathQueue::default();
        path.observe(Some(acked(1_000, FULL, 0)), 0);
        path.observe(
            Some(busy_progress(acked(1_000, FULL, 10), 100_000)),
            100_000,
        );
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

    #[test]
    fn rereading_a_queued_rtt_never_teaches_a_longer_route() {
        let mut path = PathQueue::default();
        path.observe(Some(acked(5_000, FULL, 1)), 0);
        let stalled = acked(400_000, FULL, 2);
        path.observe(Some(stalled), 10_000);
        for now in (20_000..=4 * BASELINE_WINDOW_US).step_by(10_000) {
            path.observe(Some(stalled), now);
            assert!(path.queued(), "no new ACK can justify a new route");
            assert_eq!(path.base_us, Some(5_000));
        }
    }

    #[test]
    fn acknowledging_older_packets_cannot_refresh_an_unchanged_rtt_estimator() {
        let mut path = PathQueue::default();
        path.observe(Some(acked(5_000, FULL, 1)), 0);
        let mut stale = acked(400_000, FULL, 2);
        path.observe(Some(stale), 10_000);
        // An ACK range can acknowledge older packets while repeating an
        // already-acknowledged largest packet. QUIC counts those ACKs but does
        // not update its RTT estimator. They cannot certify a route change.
        for now in (20_000..=4 * BASELINE_WINDOW_US).step_by(10_000) {
            stale.packets_acked += 1;
            path.observe(Some(stale), now);
            assert!(path.queued());
            assert_eq!(path.base_us, Some(5_000));
        }
    }

    #[test]
    fn a_drain_probe_needs_new_acknowledgements_before_adopting_a_route() {
        let mut path = PathQueue::default();
        path.observe(Some(acked(5_000, FULL, 1)), 0);
        path.observe(Some(acked(400_000, FULL, 2)), 10_000);
        path.observe(Some(acked(400_000, 0, 3)), 20_000);
        assert!(path.probe.is_some());
        // The peer is silent after its last queued packet was acknowledged.
        // Repeatedly reading that RTT after two round trips cannot complete
        // the probe. Starting new traffic must still respect the old baseline.
        for now in (30_000..=900_000).step_by(10_000) {
            path.observe(Some(acked(400_000, 0, 3)), now);
        }
        path.observe(Some(acked(400_000, FULL, 3)), 910_000);
        assert!(path.queued());
        assert_eq!(path.base_us, Some(5_000));
        // The first new answer from the drained path proves the queue was
        // transient; no controller needs to undo a fabricated route change.
        path.observe(Some(acked(6_000, FULL, 4)), 920_000);
        assert!(!path.queued());
        assert_eq!(path.base_us, Some(5_000));
    }

    #[test]
    fn sparse_new_round_trips_cannot_fill_a_continuous_route_window() {
        let mut path = PathQueue::default();
        let mut load = acked(5_000, FULL, 1);
        path.observe(Some(load), 0);
        let mut previous_at = 0;
        for (now, rtt, packets) in [
            (10_000, 400_000, 2),
            (5_000_000, 410_000, 3),
            (10_000_000, 420_000, 4),
            (15_000_000, 430_000, 5),
        ] {
            for repeated_at in (previous_at + 10_000..now).step_by(10_000) {
                path.observe(Some(load), repeated_at);
            }
            load = acked(rtt, FULL, packets);
            path.observe(Some(load), now);
            assert_eq!(path.base_us, Some(5_000));
            assert!(path.queued(), "stale time cannot establish a longer route");
            previous_at = now;
        }
    }

    #[test]
    fn only_recent_acknowledged_busy_traffic_supplies_clear_headroom() {
        let mut path = PathQueue::default();
        assert_eq!(current(&path), PathEvidence::Unknown);
        path.observe(Some(acked(5_000, 0, 1)), 0);
        assert_eq!(current(&path), PathEvidence::Idle);
        // Idle heartbeats carry no evidence that a video bitrate fits.
        path.observe(Some(acked(5_000, 0, 2)), 10_000);
        assert_eq!(current(&path), PathEvidence::Idle);
        path.observe(Some(acked(5_000, FULL, 2)), 20_000);
        assert_eq!(current(&path), PathEvidence::Idle);
        path.observe(Some(busy_progress(acked(5_000, 0, 3), 30_000)), 30_000);
        assert_eq!(current(&path), PathEvidence::Clear);
        path.observe(Some(acked(5_000, 0, 3)), 30_000 + HEADROOM_EVIDENCE_US);
        assert_eq!(current(&path), PathEvidence::Clear);
        path.observe(Some(acked(5_000, 0, 4)), 30_001 + HEADROOM_EVIDENCE_US);
        assert_eq!(current(&path), PathEvidence::Idle);
        path.observe(None, 300_000);
        assert_eq!(current(&path), PathEvidence::Unknown);
    }

    #[test]
    fn the_first_high_rtt_after_a_drained_idle_cannot_certify_a_longer_route() {
        let mut path = PathQueue::default();
        path.observe(Some(acked(5_000, FULL, 1)), 0);
        path.observe(Some(acked(400_000, FULL, 2)), 10_000);
        let drained = acked(400_000, 0, 3);
        path.observe(Some(drained), 20_000);
        for now in (50_000..=900_000).step_by(50_000) {
            path.observe(Some(drained), now);
        }
        // A new large burst can be congested even after an idle interval. Its
        // first ACK is not two observed round trips from an empty path.
        path.observe(Some(acked(400_000, FULL, 3)), 910_000);
        path.observe(Some(acked(405_000, FULL, 4)), 1_310_000);
        assert!(path.queued());
        assert_eq!(path.base_us, Some(5_000));
    }

    #[test]
    fn an_idle_gap_between_fresh_answers_restarts_a_route_probe() {
        let mut path = PathQueue::default();
        sample(&mut path, 5_000, FULL, 0);
        sample(&mut path, 400_000, FULL, 10_000);
        sample(&mut path, 400_000, 0, 20_000);
        sample(&mut path, 405_000, FULL, 100_000);
        let last = acked(405_000, FULL, 4);
        for now in (150_000..=1_000_000).step_by(50_000) {
            path.observe(Some(last), now);
        }
        sample(&mut path, 406_000, FULL, 1_010_001);
        assert!(path.queued(), "idle time cannot finish the original probe");
        assert_eq!(path.base_us, Some(5_000));
        for now in [1_200_000, 1_500_000, 1_824_002] {
            sample(&mut path, 407_000, FULL, now);
        }
        assert_eq!(path.base_us, Some(406_000));
        assert!(!path.queued(), "a new continuous observation can finish");
    }

    #[test]
    fn a_busy_flight_that_drains_between_path_samples_still_proves_headroom() {
        let mut path = PathQueue::default();
        for now in (0..=5_000_000).step_by(50_000) {
            // The transport sampled the actual busy flight immediately after
            // flush. Its packets are already ACKed by this subscriber turn.
            let packets = now / 50_000 + 10;
            path.observe(Some(busy_progress(acked(5_000, 0, packets), now)), now);
            assert_eq!(path.evidence(now), PathEvidence::Clear);
            assert!(!path.queued());
        }
        let last = busy_progress(acked(5_000, 0, 111), 5_000_000);
        path.observe(Some(last), 5_250_001);
        assert_eq!(current(&path), PathEvidence::Idle);
    }

    #[test]
    fn delayed_or_future_native_progress_is_never_retimed_on_receipt() {
        for at in [1, 1_000_001] {
            let mut path = PathQueue::default();
            let progress = busy_progress(acked(5_000, 0, 10), at);
            path.observe(Some(progress), 1_000_000);
            assert_eq!(current(&path), PathEvidence::Idle);
            // A previously future token cannot become newly observed proof
            // when the consumer catches up, and a late token cannot renew.
            path.observe(Some(progress), 1_000_002);
            assert_eq!(current(&path), PathEvidence::Idle);
        }
    }

    #[test]
    fn unavailable_telemetry_cannot_bridge_delivery_or_route_evidence() {
        let mut path = measured_at_120_kb_per_second();
        assert_eq!(current(&path), PathEvidence::Clear);
        path.observe(None, 150_000);
        assert_eq!(path.delivered_bits_per_second(), 0);
        path.observe(Some(acked(200_000, FULL, 100)), 200_000);
        assert_eq!(current(&path), PathEvidence::Idle);
        assert_eq!(path.delivered_bits_per_second(), 0);
        assert_eq!(path.base_us, Some(200_000));
        assert!(path.probe.is_none());
    }

    #[test]
    fn a_shared_consumers_reads_cannot_refresh_an_unserviced_viewers_sample() {
        let mut path = measured_at_120_kb_per_second();
        assert_eq!(path.evidence(100_000), PathEvidence::Clear);
        assert_eq!(
            path.evidence(100_000 + SAMPLE_EVIDENCE_US),
            PathEvidence::Clear
        );
        for now in [99_999, 100_001 + SAMPLE_EVIDENCE_US, 10_000_000] {
            assert_eq!(path.evidence(now), PathEvidence::Unknown);
        }
        path.observe(Some(acked(400_000, FULL, 11)), 110_000);
        assert!(matches!(
            path.evidence(110_000),
            PathEvidence::Queued { .. }
        ));
        assert_eq!(
            path.evidence(110_001 + SAMPLE_EVIDENCE_US),
            PathEvidence::Unknown
        );
    }

    #[test]
    fn counter_and_clock_discontinuities_discard_advisory_history() {
        for (now, packets) in [(110_000, 1), (99_999, 11)] {
            let mut path = measured_at_120_kb_per_second();
            path.observe(Some(acked(200_000, FULL, packets)), now);
            assert_eq!(current(&path), PathEvidence::Idle);
            assert_eq!(path.delivered_bits_per_second(), 0);
            assert_eq!(path.base_us, Some(200_000));
            assert!(path.probe.is_none());
        }
    }

    #[test]
    fn an_unobserved_service_gap_cannot_supply_busy_time_or_headroom() {
        let mut path = measured_at_120_kb_per_second();
        let resume = 100_001 + RATE_WINDOW_US;
        path.observe(Some(acked(5_000, FULL, 1_000)), resume);
        assert_eq!(current(&path), PathEvidence::Idle);
        assert_eq!(path.delivered_bits_per_second(), 0);
        assert_eq!(path.base_us, Some(1_000), "known route stays conservative");
        path.observe(
            Some(busy_progress(
                acked(5_000, FULL, 1_010),
                resume + MIN_BUSY_US,
            )),
            resume + MIN_BUSY_US,
        );
        assert_eq!(current(&path), PathEvidence::Clear);
        assert_eq!(path.delivered_bits_per_second(), 120_000 * 8);
    }
}
