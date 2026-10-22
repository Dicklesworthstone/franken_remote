//! Outcome of the final application-record gate, distinct from connection authority.

/// A synchronous prepared-record send either admits the whole record or leaves
/// it with its application owner. Neither outcome proves remote delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendAdmission<E> {
    /// The unchanged record entered the bounded transport queue.
    Accepted,
    /// No byte of this record entered a transport queue. The original
    /// connection and previously admitted records remain owned by the caller.
    /// The application decides whether this typed refusal permits recovery;
    /// it must never treat an authority or malformed-record error as expiry.
    Refused(E),
}

use super::{Messages, Policy, Priority, QuicRecords, StreamRoute};
use fr_wire::Kind;

// Within, never in addition to, the existing critical allowance. Leave room
// for two small renewal records at the default policy. Tiny local profiles
// reserve no more than a quarter of either original limit.
const RESERVED_RECORDS: usize = 2;
const RESERVED_BYTES: usize = 512;

impl QuicRecords {
    /// Called after framing, route and total-budget validation, before copying.
    /// Count ALL retained critical records, including native-unacknowledged
    /// epochs. A prefix flush or membership in a batch cannot reclaim room.
    pub(super) fn metadata_admission_ready(
        &self,
        route: StreamRoute,
        bytes: &[u8],
        used_bytes: usize,
        used_records: usize,
    ) -> bool {
        !limited_metadata(route, bytes, &self.streams)
            || metadata_fits(self.policy, (used_bytes, used_records), bytes.len())
    }
}

fn limited_metadata(route: StreamRoute, bytes: &[u8], streams: &[StreamRoute]) -> bool {
    // Classify the RECORD, not its lane: PresentedState and renewal responses
    // share SessionControl. A per-lane reservation penalizes both equally.
    route.priority == Priority::Critical
        && bytes.get(6..8).is_some_and(|kind| {
            let kind = u16::from_be_bytes([kind[0], kind[1]]);
            kind == Kind::Progress as u16 || kind == Kind::PresentedState as u16
        })
        && streams.iter().any(|stream| {
            stream.outbound
                && stream.priority == Priority::Critical
                && stream.messages == Messages::SessionControl
        })
}

fn metadata_fits(policy: Policy, used: (usize, usize), length: usize) -> bool {
    let (bytes, records) = used;
    // The caller already enforces the full budget. Keep the pure boundary
    // independently conservative, including arithmetic faults and small caps.
    let Some(total) = bytes.checked_add(length) else {
        return false;
    };
    if records >= policy.critical_send_records || total > policy.critical_send_bytes {
        return false;
    }
    // Never permanently exclude a whole record admitted by a tiny/custom
    // profile. It may start an EMPTY flight even if it uses the byte reserve;
    // subsequent metadata waits for actual ACK reclamation. This exception is
    // not an extra allowance and does not extend the record's deadline.
    if bytes == 0 && records == 0 {
        return true;
    }
    let record_limit =
        policy.critical_send_records - (policy.critical_send_records / 4).min(RESERVED_RECORDS);
    let byte_limit =
        policy.critical_send_bytes - (policy.critical_send_bytes / 4).min(RESERVED_BYTES);
    records < record_limit && total <= byte_limit
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quic::StreamId;

    fn route(messages: Messages) -> StreamRoute {
        StreamRoute {
            stream: StreamId(2),
            binding: 1,
            messages,
            priority: Priority::Critical,
            outbound: true,
            maximum: 8192,
        }
    }
    fn header(kind: u16) -> [u8; fr_wire::HEADER_BYTES] {
        let mut bytes = [0; fr_wire::HEADER_BYTES];
        bytes[6..8].copy_from_slice(&kind.to_be_bytes());
        bytes
    }

    #[test]
    fn only_frame_metadata_is_limited_even_on_a_mixed_control_lane() {
        let control = route(Messages::SessionControl);
        for kind in 0..=u16::MAX {
            let expected = kind == Kind::Progress as u16 || kind == Kind::PresentedState as u16;
            assert_eq!(
                limited_metadata(control, &header(kind), &[control]),
                expected
            );
        }
        assert!(!limited_metadata(control, &[], &[control]));
        assert!(!limited_metadata(control, &[0; 7], &[control]));
    }

    #[test]
    fn reservation_needs_an_outbound_critical_session_control_lane() {
        let progress = route(Messages::Exact(Kind::Progress as u16));
        let control = route(Messages::SessionControl);
        let bytes = header(Kind::Progress as u16);
        assert!(!limited_metadata(progress, &bytes, &[progress]));
        assert!(limited_metadata(progress, &bytes, &[progress, control]));
        for other in [
            StreamRoute {
                outbound: false,
                ..control
            },
            StreamRoute {
                priority: Priority::Bulk,
                ..control
            },
            route(Messages::Negotiation),
        ] {
            assert!(!limited_metadata(progress, &bytes, &[other]));
        }
        assert!(!limited_metadata(
            StreamRoute {
                priority: Priority::Bulk,
                ..progress
            },
            &bytes,
            &[control],
        ));
    }

    #[test]
    fn default_record_reserve_is_shared_across_all_critical_traffic() {
        let policy = Policy::default();
        for records in 0..14 {
            assert!(metadata_fits(policy, (records * 64, records), 64));
        }
        for records in 14..=16 {
            assert!(!metadata_fits(policy, (records * 64, records), 64));
        }
        // Two renewals already retained cannot be subtracted as if free: all
        // traffic competes for the SAME original budget, regardless of lane.
        assert!(!metadata_fits(policy, (12 * 64 + 2 * 96, 14), 64));
    }

    #[test]
    fn byte_reserve_is_independent_of_record_room_and_refuses_atomically() {
        let policy = Policy::default();
        let used = (8192 - RESERVED_BYTES - 64, 2);
        assert!(metadata_fits(policy, used, 64));
        assert!(!metadata_fits(policy, used, 65));
        for _ in 0..1000 {
            assert!(!metadata_fits(policy, (8192 - RESERVED_BYTES, 2), 24));
        }
        assert!(metadata_fits(policy, used, 64));
        assert!(!metadata_fits(policy, (8192, 1), 24));
        assert!(!metadata_fits(policy, (usize::MAX, 1), 24));
    }

    #[test]
    fn tiny_profiles_keep_whole_record_progress_without_extra_credit() {
        for records in 1..=3 {
            let policy = Policy {
                critical_send_bytes: 128,
                critical_send_records: records,
                ..Policy::default()
            };
            assert!(metadata_fits(policy, (0, 0), 128));
            assert!(!metadata_fits(policy, (0, 0), 129));
            assert!(!metadata_fits(policy, (128, 1), 24));
            // No record slot is reserved at these small count limits.
            for count in 0..records {
                assert!(metadata_fits(policy, (count * 24, count), 24));
            }
            assert!(!metadata_fits(policy, (records * 24, records), 24));
        }
        let policy = Policy {
            critical_send_bytes: 24,
            ..Policy::default()
        };
        assert!(metadata_fits(policy, (0, 0), 24));
        assert!(!metadata_fits(policy, (24, 1), 24));
    }

    #[test]
    fn empty_flight_exception_never_applies_to_an_outstanding_record() {
        let policy = Policy::default();
        assert!(metadata_fits(policy, (0, 0), 8192));
        assert!(!metadata_fits(policy, (0, 0), 8193));
        assert!(!metadata_fits(policy, (24, 1), 8168));
        // Inconsistent usage also cannot claim the empty-flight exception.
        assert!(!metadata_fits(policy, (0, 1), 8192));
        assert!(!metadata_fits(policy, (24, 0), 8168));
        assert!(!metadata_fits(policy, (0, usize::MAX), 24));
    }
}
