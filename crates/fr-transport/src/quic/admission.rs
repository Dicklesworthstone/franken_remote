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

    /// Leave one complete recovery record inside the existing Bulk allowance.
    /// Retained recovery records count against the same allowance as files and
    /// clipboard data; prefix flushing is not acknowledgement or free capacity.
    pub(super) fn recovery_admission_ready(
        &self,
        route: StreamRoute,
        length: usize,
        used_bytes: usize,
        used_records: usize,
    ) -> bool {
        if route.priority != Priority::Bulk || recovery_route(route) {
            return true;
        }
        let maximum = recovery_maximum(&self.streams, |stream| {
            self.has_route(super::Route::Stream(stream))
                && self.native.as_ref().is_some_and(|native| {
                    native
                        .connection()
                        .inner()
                        .streams()
                        .stream(stream.stream)
                        .is_ok_and(|live| {
                            live.send_final_size.is_none()
                                && live.send_reset.is_none()
                                && live.stop_sending_error_code.is_none()
                        })
                })
        });
        bulk_recovery_fits(self.policy, (used_bytes, used_records), length, maximum)
    }
}

fn recovery_route(route: StreamRoute) -> bool {
    route.outbound
        && route.priority == Priority::Bulk
        && route.messages == Messages::Exact(Kind::Recovery as u16)
}

fn recovery_maximum(
    streams: &[StreamRoute],
    mut live: impl FnMut(StreamRoute) -> bool,
) -> Option<usize> {
    // The route list retains retired attachment tombstones. Reserve only for
    // live senders, and only for ONE largest record, not every recovery lane.
    streams
        .iter()
        .copied()
        .filter(|route| recovery_route(*route) && live(*route))
        .map(|route| route.maximum)
        .max()
}

fn bulk_recovery_fits(
    policy: Policy,
    used: (usize, usize),
    length: usize,
    recovery_maximum: Option<usize>,
) -> bool {
    let (bytes, records) = used;
    let Some(total) = bytes.checked_add(length) else {
        return false;
    };
    if records >= policy.retained_send_records || total > policy.retained_send_bytes {
        return false;
    }
    let Some(maximum) = recovery_maximum else {
        return true;
    };
    // A large record or tiny/custom profile may borrow the reserve to start
    // an EMPTY flight. This prevents permanent backpressure without raising a
    // quota, admitting a second borrower, or extending any original deadline.
    // Admission isolation cannot preempt already retained records or promise
    // congestion throughput while this bounded flight awaits acknowledgement.
    if bytes == 0 && records == 0 {
        return true;
    }
    let byte_limit = policy.retained_send_bytes - maximum.min(policy.retained_send_bytes);
    let record_limit = policy.retained_send_records.saturating_sub(1);
    total <= byte_limit && records < record_limit
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

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use crate::quic::StreamId;

    fn route(binding: u32, maximum: usize) -> StreamRoute {
        StreamRoute {
            stream: StreamId(3),
            binding,
            messages: Messages::Exact(Kind::Recovery as u16),
            priority: Priority::Bulk,
            outbound: true,
            maximum,
        }
    }

    #[test]
    fn only_exact_outbound_bulk_recovery_owns_headroom() {
        let recovery = route(1, 8192);
        for kind in 0..=u16::MAX {
            assert_eq!(
                recovery_route(StreamRoute {
                    messages: Messages::Exact(kind),
                    ..recovery
                }),
                kind == Kind::Recovery as u16
            );
        }
        for other in [
            StreamRoute {
                outbound: false,
                ..recovery
            },
            StreamRoute {
                priority: Priority::Critical,
                ..recovery
            },
            StreamRoute {
                messages: Messages::Files,
                ..recovery
            },
            StreamRoute {
                messages: Messages::Clipboard,
                ..recovery
            },
        ] {
            assert!(!recovery_route(other));
        }
    }

    #[test]
    fn reserve_tracks_largest_live_route_without_summing_or_retaining_tombstones() {
        let small = route(1, 1024);
        let large = route(2, 4096);
        let retired = route(3, 65_536);
        let inbound = StreamRoute {
            outbound: false,
            ..route(4, 65_536)
        };
        let critical = StreamRoute {
            priority: Priority::Critical,
            ..route(5, 65_536)
        };
        let streams = [small, large, retired, inbound, critical];
        assert_eq!(
            recovery_maximum(&streams, |r| r.binding != retired.binding),
            Some(4096)
        );
        assert_eq!(
            recovery_maximum(&[small, large, large], |_| true),
            Some(4096)
        );
        assert_eq!(recovery_maximum(&streams, |_| false), None);
        assert_eq!(recovery_maximum(&[inbound, critical], |_| true), None);
        assert_eq!(recovery_maximum(&[], |_| true), None);
    }

    #[test]
    fn default_byte_reserve_has_an_exact_boundary_independent_of_record_room() {
        let policy = Policy::default();
        let reserve = Some(8192);
        let used = (policy.retained_send_bytes - 8192 - 24, 3);
        assert!(bulk_recovery_fits(policy, used, 24, reserve));
        assert!(!bulk_recovery_fits(policy, used, 25, reserve));
        for _ in 0..1000 {
            assert!(!bulk_recovery_fits(
                policy,
                (policy.retained_send_bytes - 8192, 1),
                24,
                reserve
            ));
        }
        assert!(bulk_recovery_fits(policy, used, 24, reserve));
    }

    #[test]
    fn one_record_slot_is_reserved_from_all_retained_bulk_records() {
        let policy = Policy::default();
        for records in 0..policy.retained_send_records - 1 {
            assert!(bulk_recovery_fits(
                policy,
                (records * 24, records),
                24,
                Some(8192)
            ));
        }
        for records in policy.retained_send_records - 1..=policy.retained_send_records {
            assert!(!bulk_recovery_fits(
                policy,
                (records * 24, records),
                24,
                Some(8192)
            ));
        }
        // A recovery record already retained is not subtracted from the used
        // totals. The next data record must still leave one original slot.
        assert!(!bulk_recovery_fits(
            policy,
            (8192 + 126 * 24, 127),
            24,
            Some(8192)
        ));
    }

    #[test]
    fn no_live_recovery_route_restores_full_original_capacity() {
        let policy = Policy::default();
        let used = (
            policy.retained_send_bytes - 24,
            policy.retained_send_records - 1,
        );
        assert!(bulk_recovery_fits(policy, used, 24, None));
        assert!(!bulk_recovery_fits(policy, used, 25, None));
        assert!(!bulk_recovery_fits(
            policy,
            (0, policy.retained_send_records),
            24,
            None
        ));
    }

    #[test]
    fn negotiated_recovery_size_changes_only_reserved_space() {
        let policy = Policy::default();
        for maximum in [24, 4096, 8192, 65_536] {
            let used = (policy.retained_send_bytes - maximum - 24, 1);
            assert!(bulk_recovery_fits(policy, used, 24, Some(maximum)));
            assert!(!bulk_recovery_fits(policy, used, 25, Some(maximum)));
        }
    }

    #[test]
    fn tiny_profiles_admit_one_empty_flight_borrower_without_permanent_backpressure() {
        for records in 1..=3 {
            let policy = Policy {
                retained_send_bytes: 128,
                retained_send_records: records,
                ..Policy::default()
            };
            assert!(bulk_recovery_fits(policy, (0, 0), 128, Some(128)));
            assert!(!bulk_recovery_fits(policy, (0, 0), 129, Some(128)));
            assert!(!bulk_recovery_fits(policy, (24, 1), 24, Some(128)));
            assert!(!bulk_recovery_fits(policy, (128, 1), 24, Some(128)));
            // Both dimensions, not merely the record count, must be empty.
            assert!(!bulk_recovery_fits(policy, (24, 0), 24, Some(128)));
            assert!(!bulk_recovery_fits(policy, (0, 1), 128, Some(128)));
        }
    }

    #[test]
    fn single_record_profiles_cannot_borrow_a_second_slot() {
        let policy = Policy {
            retained_send_records: 1,
            ..Policy::default()
        };
        assert!(bulk_recovery_fits(policy, (0, 0), 24, Some(8192)));
        assert!(!bulk_recovery_fits(policy, (24, 1), 24, Some(8192)));
    }

    #[test]
    fn arithmetic_faults_or_reservation_sizes_never_raise_original_quotas() {
        let policy = Policy::default();
        for reserve in [None, Some(24), Some(8192), Some(usize::MAX)] {
            assert!(!bulk_recovery_fits(policy, (usize::MAX, 1), 24, reserve));
            assert!(!bulk_recovery_fits(policy, (0, usize::MAX), 24, reserve));
            assert!(!bulk_recovery_fits(
                policy,
                (0, 0),
                policy.retained_send_bytes + 1,
                reserve
            ));
            assert!(!bulk_recovery_fits(
                policy,
                (policy.retained_send_bytes, 1),
                24,
                reserve
            ));
        }
    }
}
