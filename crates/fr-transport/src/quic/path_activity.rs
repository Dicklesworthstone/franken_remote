//! Acknowledged progress from actual busy or video flights at native I/O
//! boundaries. A burst can flush and drain between the caller's path samples;
//! small video frames can fit below the busy threshold. Admission, cursor
//! updates and idle ACK counts alone cannot prove video delivery headroom.

pub(super) const BUSY_BYTES: u64 = 4 * 1200;

#[derive(Debug, Default)]
pub(super) struct Activity {
    last_acked: u64,
    last_lost: u64,
    last_at: Option<u64>,
    busy: bool,
    /// Native emitted/drop/loss counters at admission of a video fragment.
    /// Loss during a partial flush cannot disappear at a later cursor flush.
    queued_video: Option<(u64, u64, u64)>,
    /// ACK counter at the completed video flush, and the original loss count.
    /// Partial ACKs may belong to older control traffic. Only a complete flight
    /// drain with new ACK progress and no loss proves delivery of the video.
    video_flight: Option<(u64, u64)>,
    progress: Option<(u64, u64)>,
}

impl Activity {
    /// Call only after full record validation and successful native admission.
    /// The installed video route also carries cursor positions (0x0039), so
    /// only the actual fragment kind (0x0034) can start this witness.
    pub(super) fn admitted_datagram(
        &mut self,
        kind: u16,
        emitted: u64,
        dropped: u64,
        packets_lost: u64,
    ) {
        if kind == 0x0034 && packets_lost != u64::MAX {
            self.queued_video
                .get_or_insert((emitted, dropped, packets_lost));
        }
    }

    /// `at` is the original I/O operation's pre-await host timestamp. A delayed
    /// completion cannot renew the age of the evidence it eventually exposes.
    pub(super) fn observe(
        &mut self,
        bytes_in_flight: u64,
        packets_acked: u64,
        packets_lost: u64,
        at: u64,
    ) {
        if packets_acked < self.last_acked
            || packets_lost < self.last_lost
            || self.last_at.is_some_and(|previous| at < previous)
        {
            *self = Self::default();
        }
        if self
            .queued_video
            .is_some_and(|(_, _, lost)| packets_lost != lost)
        {
            self.queued_video = None;
        }
        let acknowledged = packets_acked > self.last_acked;
        let video_delivered = self.video_flight.is_some_and(|(acked, lost)| {
            bytes_in_flight == 0 && packets_acked > acked && packets_lost == lost
        });
        if packets_lost != self.last_lost || packets_lost == u64::MAX {
            // A mixed ACK/loss turn cannot certify spare capacity. A new
            // path observer must not inherit an older pre-loss ACK either.
            self.progress = None;
        } else if (self.busy || video_delivered) && acknowledged {
            self.progress = Some((packets_acked, at));
        }
        if bytes_in_flight == 0
            || self
                .video_flight
                .is_some_and(|(_, lost)| packets_lost != lost)
        {
            self.video_flight = None;
        }
        self.last_acked = packets_acked;
        self.last_lost = packets_lost;
        self.last_at = Some(at);
        self.busy = bytes_in_flight > BUSY_BYTES;
    }

    /// Call after `observe` for a successfully completed native I/O operation.
    /// `sent` comes from its actual packet-send result. A nonempty queue, an
    /// assembly-time drop, or a flush that sent nothing cannot certify video.
    /// The new flight can only qualify ACKs from a subsequent operation.
    pub(super) fn flushed(
        &mut self,
        bytes_in_flight: u64,
        sent: bool,
        queued: usize,
        emitted: u64,
        dropped: u64,
    ) {
        let Some((before, drops, lost)) = self.queued_video else {
            return;
        };
        let counters_match = emitted >= before && dropped == drops && self.last_lost == lost;
        if queued != 0 && counters_match {
            return;
        }
        self.queued_video = None;
        if counters_match && queued == 0 && sent && emitted > before && bytes_in_flight > 0 {
            self.video_flight = Some((self.last_acked, lost));
        }
    }

    pub(super) const fn progress(&self) -> Option<(u64, u64)> {
        self.progress
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flush_and_fully_drained_receive_preserve_the_busy_flight() {
        let mut activity = Activity::default();
        activity.observe(0, 1, 0, 10);
        activity.observe(12_000, 1, 0, 20); // actual post-flush flight
        assert_eq!(activity.progress(), None);
        activity.observe(0, 11, 0, 30); // fully drained before the caller samples
        assert_eq!(activity.progress(), Some((11, 30)));
    }

    #[test]
    fn idle_acks_repeated_counters_and_loss_cannot_manufacture_busy_progress() {
        let mut activity = Activity::default();
        for acked in 1..100 {
            activity.observe(BUSY_BYTES, acked, 0, acked);
            assert_eq!(activity.progress(), None);
        }
        activity.observe(BUSY_BYTES + 1, 100, 0, 100);
        activity.observe(0, 100, 0, 110); // declared lost, not acknowledged
        activity.observe(0, 101, 0, 120); // later idle ACK
        assert_eq!(activity.progress(), None);
    }

    #[test]
    fn partial_busy_delivery_with_loss_retires_the_old_positive_witness() {
        let mut activity = Activity::default();
        activity.observe(12_000, 10, 0, 100);
        activity.observe(12_000, 12, 0, 110);
        assert_eq!(activity.progress(), Some((12, 110)));
        activity.observe(12_000, 14, 1, 120);
        assert_eq!(activity.progress(), None);
        activity.observe(600, 15, 1, 130);
        assert_eq!(activity.progress(), Some((15, 130)));
        activity.observe(600, 15, 2, 140);
        assert_eq!(activity.progress(), None, "loss without a new ACK");
    }

    #[test]
    fn repeated_idle_reads_keep_the_original_progress_time() {
        let mut activity = Activity::default();
        activity.observe(12_000, 10, 0, 100);
        activity.observe(0, 20, 0, 110);
        for now in [111, 200, 10_000_000] {
            activity.observe(0, 21, 0, now);
            assert_eq!(activity.progress(), Some((20, 110)));
        }
    }

    #[test]
    fn busy_progress_is_scoped_to_monotonic_native_counter_and_time() {
        for (packets, now) in [(1, 120), (21, 109)] {
            let mut activity = Activity::default();
            activity.observe(12_000, 10, 0, 100);
            activity.observe(12_000, 20, 0, 110);
            activity.observe(0, packets, 0, now);
            assert_eq!(activity.progress(), None);
        }
    }

    #[test]
    fn clock_regression_before_first_progress_discards_the_pending_flight() {
        let mut activity = Activity::default();
        activity.observe(12_000, 10, 0, 100);
        activity.observe(0, 20, 0, 90);
        assert_eq!(activity.progress(), None);
    }

    #[test]
    fn clock_regression_is_checked_against_nonprogress_samples_too() {
        let mut activity = Activity::default();
        activity.observe(12_000, 10, 0, 100);
        activity.observe(0, 20, 0, 110);
        activity.observe(12_000, 20, 0, 200);
        activity.observe(0, 21, 0, 150);
        assert_eq!(activity.progress(), None);
    }

    #[test]
    fn a_small_video_frame_needs_actual_flush_then_a_later_ack() {
        let mut activity = Activity::default();
        activity.observe(0, 10, 0, 100);
        activity.admitted_datagram(0x0034, 0, 0, 0);
        assert_eq!(
            activity.progress(),
            None,
            "admission proves no transmission"
        );
        // ACKs in this operation precede its outbound flush. They cannot
        // acknowledge the small frame that the same operation just sent.
        activity.observe(600, 11, 0, 110);
        activity.flushed(600, true, 0, 1, 0);
        assert_eq!(activity.progress(), None);
        activity.observe(0, 12, 0, 120);
        assert_eq!(activity.progress(), Some((12, 120)));
    }

    #[test]
    fn cursor_and_other_datagrams_cannot_certify_small_video_flights() {
        for kind in [0x0039, 0x0042, 0x0060, 0x0001] {
            let mut activity = Activity::default();
            activity.observe(0, 10, 0, 100);
            activity.admitted_datagram(kind, 0, 0, 0);
            activity.observe(600, 10, 0, 110);
            activity.flushed(600, true, 0, 1, 0);
            activity.observe(0, 11, 0, 120);
            assert_eq!(activity.progress(), None, "record kind {kind:#06x}");
        }
    }

    #[test]
    fn admission_partial_flush_drops_and_empty_flights_supply_no_video_proof() {
        for (bytes, sent, queued, emitted, dropped) in [
            (600, false, 1, 10, 0), // admitted, cwnd blocked
            (600, true, 1, 11, 0),  // only part of the queue flushed
            (600, false, 0, 11, 0), // no successful actual send
            (600, true, 0, 10, 0),  // empty queue without emission
            (600, true, 0, 11, 1),  // a queued datagram was dropped
            (600, true, 0, 9, 0),   // native counter discontinuity
            (0, true, 0, 11, 0),    // no actual flight to acknowledge
        ] {
            let mut activity = Activity::default();
            activity.observe(0, 10, 0, 100);
            activity.admitted_datagram(0x0034, 10, 0, 0);
            activity.observe(bytes, 10, 0, 110);
            activity.flushed(bytes, sent, queued, emitted, dropped);
            activity.observe(0, 11, 0, 120);
            assert_eq!(activity.progress(), None);
        }
    }

    #[test]
    fn a_partially_flushed_video_queue_can_qualify_only_after_it_really_drains() {
        let mut activity = Activity::default();
        activity.observe(0, 10, 0, 100);
        activity.admitted_datagram(0x0034, 20, 0, 0);
        activity.observe(600, 10, 0, 110);
        activity.flushed(600, true, 1, 21, 0);
        activity.observe(0, 11, 0, 120);
        assert_eq!(activity.progress(), None);
        activity.observe(600, 11, 0, 130);
        activity.flushed(600, true, 0, 22, 0);
        assert_eq!(activity.progress(), None);
        activity.observe(0, 12, 0, 140);
        assert_eq!(activity.progress(), Some((12, 140)));
    }

    #[test]
    fn a_small_video_flight_is_consumed_only_after_its_complete_drain() {
        for acknowledged in [false, true] {
            let mut activity = Activity::default();
            activity.observe(0, 10, 0, 100);
            activity.admitted_datagram(0x0034, 0, 0, 0);
            activity.observe(600, 10, 0, 110);
            activity.flushed(600, true, 0, 1, 0);
            if acknowledged {
                activity.observe(0, 11, 0, 120);
            } else {
                activity.observe(0, 10, 0, 120); // lost without acknowledgement
            }
            let expected = acknowledged.then_some((11, 120));
            for (acked, now) in [(12, 130), (14, 140), (16, 1_000_000)] {
                activity.admitted_datagram(0x0039, 1, 0, 0);
                activity.observe(600, acked, 0, now);
                activity.flushed(600, true, 0, 2, 0);
                activity.observe(0, acked + 1, 0, now + 1);
                assert_eq!(activity.progress(), expected);
            }
        }
    }

    #[test]
    fn an_old_control_ack_cannot_acknowledge_a_small_video_flight() {
        let mut activity = Activity::default();
        activity.observe(300, 10, 0, 100); // older control packet
        activity.admitted_datagram(0x0034, 0, 0, 0);
        activity.observe(900, 10, 0, 110);
        activity.flushed(900, true, 0, 1, 0);
        activity.observe(600, 11, 0, 120); // only the old control was ACKed
        assert_eq!(activity.progress(), None);
        activity.observe(600, 11, 0, 130);
        assert_eq!(activity.progress(), None, "partial ACKs are not headroom");
        activity.observe(0, 12, 0, 140); // the video actually drains
        assert_eq!(activity.progress(), Some((12, 140)));
        activity.observe(0, 13, 0, 150);
        assert_eq!(activity.progress(), Some((12, 140)));
    }

    #[test]
    fn video_loss_plus_control_ack_cannot_supply_small_video_headroom() {
        for control_first in [false, true] {
            let mut activity = Activity::default();
            activity.observe(300, 10, 7, 100);
            activity.admitted_datagram(0x0034, 0, 0, 7);
            activity.observe(900, 10, 7, 110);
            activity.flushed(900, true, 0, 1, 0);
            if control_first {
                activity.observe(600, 11, 7, 120);
                assert_eq!(activity.progress(), None);
            }
            // Whether the control ACK precedes the video loss or they occur
            // together, a drained flight is not proof of video delivery.
            activity.observe(0, 11, 8, 130);
            assert_eq!(activity.progress(), None);
            activity.observe(0, 12, 8, 140);
            assert_eq!(activity.progress(), None);
        }
    }

    #[test]
    fn loss_during_partial_flush_cannot_be_hidden_by_a_later_cursor_flush() {
        let mut activity = Activity::default();
        activity.observe(300, 10, 7, 100);
        activity.admitted_datagram(0x0034, 0, 0, 7);
        activity.admitted_datagram(0x0039, 0, 0, 7);
        activity.observe(900, 10, 7, 110);
        activity.flushed(900, true, 1, 1, 0); // video emitted, cursor still queued
        activity.observe(100, 11, 8, 120); // video lost, old control ACKed
        activity.flushed(100, true, 0, 2, 0); // only cursor was sent this turn
        activity.observe(0, 12, 8, 130);
        assert_eq!(activity.progress(), None);
    }

    #[test]
    fn an_unchanged_nonzero_loss_counter_allows_new_video_to_recover() {
        let mut activity = Activity::default();
        activity.observe(0, 10, 7, 100);
        activity.admitted_datagram(0x0034, 0, 0, 7);
        activity.observe(600, 10, 7, 110);
        activity.flushed(600, true, 0, 1, 0);
        activity.observe(0, 11, 7, 120);
        assert_eq!(activity.progress(), Some((11, 120)));
    }

    #[test]
    fn regressed_or_saturated_loss_counters_cannot_certify_small_video_delivery() {
        for lost in [7, u64::MAX] {
            let mut activity = Activity::default();
            activity.observe(0, 10, lost, 100);
            activity.admitted_datagram(0x0034, 0, 0, lost);
            activity.observe(600, 10, lost, 110);
            activity.flushed(600, true, 0, 1, 0);
            let at_ack = if lost == u64::MAX { lost } else { lost - 1 };
            activity.observe(0, 11, at_ack, 120);
            assert_eq!(activity.progress(), None);
        }
    }

    #[test]
    fn continuously_delivered_small_video_frames_keep_their_original_ack_times() {
        let mut activity = Activity::default();
        activity.observe(0, 10, 0, 0);
        for frame in 1..=100 {
            let at = frame * 200_000;
            activity.admitted_datagram(0x0034, frame - 1, 0, 0);
            activity.observe(300, 9 + frame, 0, at);
            activity.flushed(300, true, 0, frame, 0);
            activity.observe(0, 10 + frame, 0, at + 1_000);
            assert_eq!(activity.progress(), Some((10 + frame, at + 1_000)));
            // Rereading the same native counters never freshens this proof.
            activity.observe(0, 10 + frame, 0, at + 100_000);
            assert_eq!(activity.progress(), Some((10 + frame, at + 1_000)));
        }
    }

    #[test]
    fn a_clock_regression_discards_queued_and_flushed_small_video_witnesses() {
        for flushed in [false, true] {
            let mut activity = Activity::default();
            activity.observe(0, 10, 0, 100);
            activity.admitted_datagram(0x0034, 0, 0, 0);
            activity.observe(600, 10, 0, 110);
            if flushed {
                activity.flushed(600, true, 0, 1, 0);
            }
            activity.observe(600, 11, 0, 105);
            activity.flushed(600, true, 0, 1, 0);
            activity.observe(0, 12, 0, 120);
            assert_eq!(activity.progress(), None);
        }
    }
}
