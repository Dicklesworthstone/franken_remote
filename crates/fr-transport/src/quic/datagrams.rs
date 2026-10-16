//! Independently bounded receive custody for admitted DATAGRAM routes.
//!
//! Video and pointer routes preserve one held record and one parked arrival:
//! partial/clock-only receives never discard reference fragments. Audio alone
//! may buffer four more packets across a briefly blocked consumer. Once that
//! fixed allowance is full, additional audio is discarded and counted instead
//! of stopping native reads for unrelated video or input. The existing native
//! examination budget still bounds work, including discarded packets.
//!
//! Audio custody is capped at the 100ms playout ceiling from FIRST application
//! receipt, never renewed by queuing, promotion or later arrivals. This is not
//! capture-age evidence or a timestamp for earlier native-network buffering.
//! Each retained packet keeps its own original bytes and deadline. A video
//! recovery can still delay audio behind a parked video fragment; this change
//! does not discard video references to solve that separate limitation.
use super::{Bytes, DatagramRoute, Disposition, Error, MAX_DATAGRAMS, Route};

// Four overflow slots across ALL audio routes, not four per peer identifier.
// The owner validates each payload against MAX_DATAGRAM_RECORD before custody,
// so this adds at most 4 * 1150 payload bytes and fixed inline metadata.
const AUDIO_OVERFLOW: usize = 4;
const AUDIO_PACKET: u16 = 0x0062;
const AUDIO_LIFETIME_US: u64 = fr_core::audio::MAX_JITTER_CEILING_MS as u64 * 1000;

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
    audio: [Option<Pending>; AUDIO_OVERFLOW],
    audio_len: usize,
    dropped_audio: u64,
}

impl PendingDatagrams {
    pub(super) fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| None),
            next: None,
            cursor: 0,
            audio: std::array::from_fn(|_| None),
            audio_len: 0,
            dropped_audio: 0,
        }
    }

    /// `true`: the owner may read another native record (this one was held,
    /// buffered, or counted as audio overflow). `false`: a non-audio route's
    /// later arrival is parked; no further native read until `waiting` clears.
    /// Existing bytes/deadlines and other routes' slots are never displaced.
    pub(super) fn hold(
        &mut self,
        route: DatagramRoute,
        bytes: Bytes,
        current: u64,
        lifetime: u64,
    ) -> Result<bool, Error> {
        let lifetime = if route.kind == AUDIO_PACKET {
            lifetime.min(AUDIO_LIFETIME_US)
        } else {
            lifetime
        };
        let until = current.checked_add(lifetime).ok_or(Error::Clock)?;
        self.expire(current);
        if self.next.is_some() {
            return Err(Error::Backpressure);
        }
        if self.slots.iter().flatten().any(|p| p.route == route) {
            if route.kind == AUDIO_PACKET {
                if self.audio_len == AUDIO_OVERFLOW {
                    self.dropped_audio = self.dropped_audio.saturating_add(1);
                } else {
                    self.audio[self.audio_len] = Some(Pending {
                        route,
                        bytes,
                        until,
                    });
                    self.audio_len += 1;
                }
                return Ok(true);
            }
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
                let route = pending.route;
                self.slots[index] = self.take_audio_for(route);
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
                if slot.as_ref().is_some_and(|p| p.route.kind == AUDIO_PACKET) {
                    self.dropped_audio = self.dropped_audio.saturating_add(1);
                }
                *slot = None;
            }
        }
        let mut index = 0;
        while index < self.audio_len {
            if self.audio[index]
                .as_ref()
                .is_some_and(|p| current >= p.until)
            {
                self.remove_audio(index);
                self.dropped_audio = self.dropped_audio.saturating_add(1);
            } else {
                index += 1;
            }
        }
        // A head can expire before a later packet. Promote its oldest live
        // successor without changing that successor's original deadline.
        for index in 0..MAX_DATAGRAMS {
            if self.slots[index].is_some() {
                continue;
            }
            if let Some(queued) = self.audio[..self.audio_len].iter().position(|p| {
                p.as_ref().is_some_and(|p| {
                    !self
                        .slots
                        .iter()
                        .flatten()
                        .any(|head| head.route == p.route)
                })
            }) {
                self.slots[index] = self.remove_audio(queued);
            }
        }
    }

    fn take_audio_for(&mut self, route: DatagramRoute) -> Option<Pending> {
        let index = self.audio[..self.audio_len]
            .iter()
            .position(|p| p.as_ref().is_some_and(|p| p.route == route))?;
        self.remove_audio(index)
    }

    fn remove_audio(&mut self, index: usize) -> Option<Pending> {
        let packet = self.audio[index].take();
        self.audio[index..self.audio_len].rotate_left(1);
        self.audio_len -= 1;
        packet
    }

    pub(super) fn retire(&mut self, binding: u32) {
        for slot in self.slots.iter_mut().chain(std::iter::once(&mut self.next)) {
            if slot.as_ref().is_some_and(|p| p.route.binding == binding) {
                *slot = None;
            }
        }
        let mut index = 0;
        while index < self.audio_len {
            if self.audio[index]
                .as_ref()
                .is_some_and(|p| p.route.binding == binding)
            {
                self.remove_audio(index);
            } else {
                index += 1;
            }
        }
    }

    pub(super) fn clear(&mut self) {
        for slot in &mut self.slots {
            *slot = None;
        }
        self.next = None;
        for packet in &mut self.audio {
            *packet = None;
        }
        self.audio_len = 0;
        self.cursor = 0;
    }

    pub(super) fn bytes(&self) -> usize {
        self.slots
            .iter()
            .chain(std::iter::once(&self.next))
            .chain(self.audio[..self.audio_len].iter())
            .flatten()
            .map(|p| p.bytes.len())
            .sum()
    }
}

impl super::QuicRecords {
    /// Audio packets discarded by the bounded receive backlog or its original
    /// local-custody deadline. Includes drops before closure; no payload data.
    pub fn dropped_audio_datagrams(&self) -> u64 {
        self.pending_datagrams.dropped_audio
    }
}

#[cfg(test)]
#[path = "datagrams/audio_tests.rs"]
mod audio_tests;

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
