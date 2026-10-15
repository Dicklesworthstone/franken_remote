//! Independently bounded receive custody for admitted DATAGRAM routes.
//!
//! A blocked consumer owns one record, not the connection's native datagram
//! queue. Keep that original record/deadline, and let other routes' held
//! records progress. A later arrival for a FULL route is parked, and no further
//! native datagram is read until its route drains: a blocked or not-ready
//! consumer (a clock-only or startup-only receive) delays later arrivals, which
//! stay in the bounded native queue, but never loses one. Dropping them lost
//! video fragments on a clean link whenever a partial receive pulled a turn of
//! datagrams. The cost: a consumer that STAYS blocked while its route keeps
//! arriving holds other routes back until its record drains or expires. In-tree
//! consumers block for one partial pass, while the input agent is busy, or (a
//! reference recovery) hold the new video generation until the decoder
//! handshake completes, about one or two round trips: audio arriving behind it
//! is delayed by that much, not lost.
//! This does not make unreliable media reliable or replace the session's
//! reference/freshness validation.
use super::{Bytes, DatagramRoute, Disposition, Error, MAX_DATAGRAMS, Route};

struct Pending {
    route: DatagramRoute,
    bytes: Bytes,
    until: u64,
}

pub(super) struct PendingDatagrams {
    // Metadata is inline and payloads are already bounded/validated by the
    // connection owner. No peer-supplied identifier causes a map allocation.
    slots: [Option<Pending>; MAX_DATAGRAMS],
    /// The arrival whose route already holds a record; reading stops behind it.
    next: Option<Pending>,
    cursor: usize,
}

impl PendingDatagrams {
    pub(super) fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| None),
            next: None,
            cursor: 0,
        }
    }

    /// Retain one validated record. `true`: held in its route's slot. `false`:
    /// its route already holds a record, so it is parked with its own deadline
    /// and the caller must read no further native datagram (see `waiting`). It
    /// never renews the original deadline or consumes another route's slot.
    pub(super) fn hold(
        &mut self,
        route: DatagramRoute,
        bytes: Bytes,
        current: u64,
        lifetime: u64,
    ) -> Result<bool, Error> {
        let until = current.checked_add(lifetime).ok_or(Error::Clock)?;
        self.expire(current);
        if self.next.is_some() {
            return Err(Error::Backpressure);
        }
        if self.slots.iter().flatten().any(|p| p.route == route) {
            self.next = Some(Pending {
                route,
                bytes,
                until,
            });
            return Ok(false);
        }
        // The owner installs at most MAX_DATAGRAMS routes, including outbound
        // routes, and retirement removes custody before admitting a replacement.
        // Refuse rather than evict a different route if that invariant is broken.
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(Error::Backpressure)?;
        *slot = Some(Pending {
            route,
            bytes,
            until,
        });
        Ok(true)
    }

    /// At most one consumed record, with bounded round-robin polling. Blocked
    /// routes retain their exact bytes but cannot prevent another ready route
    /// from consuming its record. Expired custody never reaches the callback.
    pub(super) fn receive_one(
        &mut self,
        current: u64,
        handler: &mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<usize, Error> {
        self.expire(current);
        for _ in 0..MAX_DATAGRAMS {
            let index = self.cursor;
            self.cursor = (self.cursor + 1) % MAX_DATAGRAMS;
            let Some(pending) = self.slots[index].as_ref() else {
                continue;
            };
            if handler(Route::Datagram(pending.route), &pending.bytes)
                .map_err(|()| Error::Handler)?
                == Disposition::Consumed
            {
                self.slots[index] = None;
                return Ok(1);
            }
        }
        Ok(0)
    }

    /// Whether a parked arrival still waits for its route, after expiry and
    /// moving it into that route's slot if the slot has drained.
    pub(super) fn waiting(&mut self, current: u64) -> bool {
        self.expire(current);
        if let Some(next) = &self.next
            && !self.slots.iter().flatten().any(|p| p.route == next.route)
            && let Some(slot) = self.slots.iter_mut().find(|slot| slot.is_none())
        {
            *slot = self.next.take();
        }
        self.next.is_some()
    }

    pub(super) fn expire(&mut self, current: u64) {
        for slot in self.slots.iter_mut().chain(std::iter::once(&mut self.next)) {
            if slot.as_ref().is_some_and(|p| current >= p.until) {
                *slot = None;
            }
        }
    }

    pub(super) fn retire(&mut self, binding: u32) {
        for slot in self.slots.iter_mut().chain(std::iter::once(&mut self.next)) {
            if slot.as_ref().is_some_and(|p| p.route.binding == binding) {
                *slot = None;
            }
        }
    }

    pub(super) fn clear(&mut self) {
        for slot in &mut self.slots {
            *slot = None;
        }
        self.next = None;
        self.cursor = 0;
    }

    pub(super) fn bytes(&self) -> usize {
        self.slots
            .iter()
            .chain(std::iter::once(&self.next))
            .flatten()
            .map(|p| p.bytes.len())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(binding: u32) -> DatagramRoute {
        DatagramRoute {
            binding,
            kind: 0x0034,
            outbound: false,
        }
    }

    fn payload(value: u8, length: usize) -> Bytes {
        Bytes::from(vec![value; length])
    }

    #[test]
    fn a_blocked_audio_route_does_not_hold_video_or_pointer_custody() {
        let audio = DatagramRoute {
            kind: 0x0062,
            ..route(1)
        };
        let video = route(2);
        let pointer = DatagramRoute {
            kind: 0x0042,
            ..route(3)
        };
        let mut inbox = PendingDatagrams::new();
        for r in [audio, video, pointer] {
            assert!(inbox.hold(r, payload(7, 24), 10, 100).unwrap());
        }
        let mut delivered = Vec::new();
        for _ in 0..2 {
            assert_eq!(
                inbox.receive_one(11, &mut |r, _| {
                    if r == Route::Datagram(audio) {
                        Ok(Disposition::Blocked)
                    } else {
                        delivered.push(r);
                        Ok(Disposition::Consumed)
                    }
                }),
                Ok(1)
            );
        }
        assert_eq!(
            delivered,
            [Route::Datagram(video), Route::Datagram(pointer)]
        );
        assert_eq!(inbox.bytes(), 24);
        assert_eq!(
            inbox.receive_one(12, &mut |r, bytes| {
                assert_eq!(r, Route::Datagram(audio));
                assert_eq!(bytes, &[7; 24]);
                Ok(Disposition::Consumed)
            }),
            Ok(1)
        );
        assert_eq!(inbox.bytes(), 0);
    }

    #[test]
    fn a_full_routes_next_arrival_is_parked_with_its_own_deadline_not_dropped() {
        let mut inbox = PendingDatagrams::new();
        assert!(inbox.hold(route(1), payload(1, 24), 10, 100).unwrap());
        // Parked behind its full route; the caller reads no further record.
        assert!(!inbox.hold(route(1), payload(2, 1150), 100, 100).unwrap());
        assert!(inbox.waiting(100));
        assert_eq!(inbox.bytes(), 24 + 1150);
        // The held record keeps its original deadline; parking renews nothing.
        assert_eq!(
            inbox.receive_one(109, &mut |_, bytes| {
                assert_eq!(bytes, &[1; 24]);
                Ok(Disposition::Blocked)
            }),
            Ok(0)
        );
        // It expires at 110; the parked arrival then takes the route's slot.
        assert!(!inbox.waiting(110));
        assert_eq!(
            inbox.receive_one(110, &mut |_, bytes| {
                assert_eq!(bytes, &[2; 1150]);
                Ok(Disposition::Consumed)
            }),
            Ok(1)
        );
        assert_eq!(inbox.bytes(), 0);
    }

    #[test]
    fn all_routes_full_still_have_fixed_count_and_byte_bounds() {
        let mut inbox = PendingDatagrams::new();
        for id in 1..=u32::try_from(MAX_DATAGRAMS).unwrap() {
            assert!(inbox.hold(route(id), payload(1, 1150), 0, 100).unwrap());
        }
        // One arrival for a full route parks; its caller stops reading, so a
        // further hold before it drains is refused, never queued without bound.
        assert!(!inbox.hold(route(1), payload(2, 1150), 1, 100).unwrap());
        for _ in 0..1024 {
            assert_eq!(
                inbox.hold(route(1), payload(2, 1150), 1, 100),
                Err(Error::Backpressure)
            );
            assert_eq!(inbox.bytes(), (MAX_DATAGRAMS + 1) * 1150);
        }
        assert_eq!(
            inbox.hold(route(99), payload(3, 24), 1, 100),
            Err(Error::Backpressure)
        );
        assert_eq!(inbox.bytes(), (MAX_DATAGRAMS + 1) * 1150);
        let mut calls = 0;
        assert_eq!(
            inbox.receive_one(1, &mut |_, _| {
                calls += 1;
                Ok(Disposition::Blocked)
            }),
            Ok(0)
        );
        assert_eq!(calls, MAX_DATAGRAMS);
    }

    #[test]
    fn continually_refilled_first_slot_cannot_starve_other_slots() {
        let mut inbox = PendingDatagrams::new();
        let routes = [route(1), route(2), route(3), route(4)];
        for r in routes {
            assert!(inbox.hold(r, payload(1, 24), 0, 100).unwrap());
        }
        for round in 0..8 {
            for expected in routes {
                assert_eq!(
                    inbox.receive_one(round, &mut |r, _| {
                        assert_eq!(r, Route::Datagram(expected));
                        Ok(Disposition::Consumed)
                    }),
                    Ok(1)
                );
                assert!(inbox.hold(expected, payload(1, 24), round, 100).unwrap());
            }
        }
    }

    #[test]
    fn retirement_removes_only_its_binding_and_clear_removes_all() {
        let mut inbox = PendingDatagrams::new();
        assert!(inbox.hold(route(1), payload(1, 24), 0, 100).unwrap());
        assert!(inbox.hold(route(2), payload(2, 48), 0, 100).unwrap());
        inbox.retire(1);
        assert_eq!(inbox.bytes(), 48);
        assert_eq!(
            inbox.receive_one(1, &mut |r, _| {
                assert_eq!(r, Route::Datagram(route(2)));
                Ok(Disposition::Blocked)
            }),
            Ok(0)
        );
        inbox.clear();
        assert_eq!(inbox.bytes(), 0);
        assert_eq!(
            inbox.receive_one(2, &mut |_, _| panic!("closed custody")),
            Ok(0)
        );
    }

    #[test]
    fn expiration_is_per_route_and_reclaims_slots_without_a_receive() {
        let mut inbox = PendingDatagrams::new();
        assert!(inbox.hold(route(1), payload(1, 24), 0, 10).unwrap());
        assert!(inbox.hold(route(2), payload(2, 48), 0, 100).unwrap());
        inbox.expire(10);
        assert_eq!(inbox.bytes(), 48);
        assert!(inbox.hold(route(3), payload(3, 24), 10, 100).unwrap());
        assert_eq!(inbox.bytes(), 72);
    }

    #[test]
    fn deadline_overflow_never_installs_a_record() {
        let mut inbox = PendingDatagrams::new();
        assert_eq!(
            inbox.hold(route(1), payload(1, 24), u64::MAX, 1),
            Err(Error::Clock)
        );
        assert_eq!(inbox.bytes(), 0);
    }

    #[test]
    fn a_handler_error_remains_terminal_to_the_connection_owner() {
        let mut inbox = PendingDatagrams::new();
        assert!(inbox.hold(route(1), payload(1, 24), 0, 100).unwrap());
        assert_eq!(
            inbox.receive_one(1, &mut |_, _| Err(())),
            Err(Error::Handler)
        );
        // receive_ready closes the owner on this result; do not label the
        // failed callback's record consumed in the meantime.
        assert_eq!(inbox.bytes(), 24);
    }
}
