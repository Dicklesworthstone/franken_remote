//! Congestion-room preflight for one bounded bulk STREAM prefix.
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
}
