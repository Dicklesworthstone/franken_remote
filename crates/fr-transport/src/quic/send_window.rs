//! Congestion-room preflight for bounded STREAM prefixes and DATAGRAMs.
//! The native transport remains the final congestion/pacing authority.
pub(super) const fn bulk_prefix_fits(room: u64, queued_datagrams: u64) -> bool {
    // The pinned profile uses at most 1200 protected bytes per packet. Reserve
    // one packet for the <=900-byte prefix and one for each queued datagram,
    // NOT a second empty packet. At cwnd=2400 that extra reservation reduced
    // reliable recovery to one outstanding packet, defeating the two-packet
    // minimum in RFC 9002 section 4.8. Critical-first selection and protected
    // connection flow credit are enforced separately in queue_stream_prefix.
    // Divide first so hostile/overflow-adjacent queue counts cannot wrap.
    room / 1200 > queued_datagrams
}

pub(super) const fn datagram_fits(
    room: u64,
    queued_datagrams: u64,
    reliable_pending: bool,
) -> bool {
    // Keep one packet for actual reliable work, including bulk recovery/file
    // records: datagrams must not fill its next staging opportunity. Without
    // that work, an empty reserved packet turns the two-packet loss floor into
    // stop-and-wait. A later critical record still stages before DATAGRAMs.
    let reserved = if reliable_pending { 1200 } else { 0 };
    bulk_prefix_fits(room.saturating_sub(reserved), queued_datagrams)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_can_fill_both_packets_of_the_minimum_window() {
        assert!(bulk_prefix_fits(2400, 0));
        assert!(bulk_prefix_fits(1200, 0));
        assert!(!bulk_prefix_fits(1199, 0));
        assert!(!bulk_prefix_fits(0, 0));
    }

    #[test]
    fn queued_datagrams_keep_their_entire_packet_reservation() {
        for queued in 0..=16 {
            let required = (queued + 1) * 1200;
            assert!(!bulk_prefix_fits(required - 1, queued));
            assert!(bulk_prefix_fits(required, queued));
            assert!(bulk_prefix_fits(required + 1, queued));
        }
        assert!(!bulk_prefix_fits(2400, 2));
        assert!(bulk_prefix_fits(2400, 1));
    }

    #[test]
    fn partial_packet_room_is_not_spendable_and_arithmetic_cannot_wrap() {
        for room in [0, 1, 1199, 1200, 1201, 2399, 2400, 12000, u64::MAX] {
            for queued in [0, 1, 2, 16, u64::MAX / 1200, u64::MAX] {
                let required = (u128::from(queued) + 1) * 1200;
                assert_eq!(bulk_prefix_fits(room, queued), u128::from(room) >= required);
            }
        }
    }

    #[test]
    fn available_room_bounds_every_queued_packet_and_one_stream_prefix() {
        // Independent wide-arithmetic oracle, including every partial packet
        // boundary in the normal sixteen-datagram queue range.
        for room in 0..=24_000_u64 {
            for queued in 0..=16 {
                let required = (u128::from(queued) + 1) * 1200;
                assert_eq!(bulk_prefix_fits(room, queued), u128::from(room) >= required);
            }
        }
    }

    #[test]
    fn video_uses_both_floor_packets_and_the_room_left_by_inflight_work() {
        assert!(datagram_fits(2400, 0, false));
        assert!(datagram_fits(2400, 1, false));
        assert!(!datagram_fits(2400, 2, false));
        // One packet already in flight does not waste the remaining packet.
        assert!(datagram_fits(1200, 0, false));
        assert!(!datagram_fits(1199, 0, false));
        // A small control flight still leaves one complete video packet.
        assert!(datagram_fits(2300, 0, false));
        assert!(!datagram_fits(2300, 1, false));
    }

    #[test]
    fn actual_reliable_work_keeps_one_packet_before_new_video_admission() {
        assert!(datagram_fits(2400, 0, true));
        assert!(!datagram_fits(2400, 1, true));
        assert!(!datagram_fits(1200, 0, true));
        assert!(!datagram_fits(2399, 0, true));
        // The same room becomes usable once that original reliable work has
        // staged; waiting for its acknowledgement reserves no empty packet.
        assert!(datagram_fits(1200, 0, false));
    }

    #[test]
    fn datagram_room_accounts_for_every_queue_and_optional_reliable_packet() {
        for room in (0..=24_000_u64).chain([u64::MAX]) {
            for queued in (0..=16_u64).chain([u64::MAX / 1200, u64::MAX]) {
                for pending in [false, true] {
                    let required = (u128::from(queued) + 1 + u128::from(pending)) * 1200;
                    assert_eq!(
                        datagram_fits(room, queued, pending),
                        u128::from(room) >= required
                    );
                }
            }
        }
    }
}
