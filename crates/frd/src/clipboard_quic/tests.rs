//! Bridge boundaries use the existing loopback TLS/QUIC attachment fixtures.
//! Native-worker cancellation/publication is covered by `worker::tests`.
use super::*;
use crate::session_startup::test_network as transport_support;
use fr_core::{clipboard::Binding, limits::ProtocolLimits};
use fr_wire::{
    attachment::MediaRole,
    clipboard::{Body, CancelReason, decode, session::Pump},
};
use transport_support::{clock, runtime};

#[path = "../../../fr-native/tests/clipboard_network/attachment.rs"]
#[allow(dead_code)]
mod attachment;
#[path = "../../../fr-native/tests/clipboard_network/authority.rs"]
#[allow(dead_code)]
mod authority;

struct Fixture {
    cx: Cx,
    link: attachment::Link,
    bridge: Bridge,
    seed: WorkerSeed,
    peer: ClipboardChannel,
    grant: fr_wire::control::Granted,
    _host: InputSession,
    _input: (quic::MediaChannel, quic::MediaChannel),
}
impl Fixture {
    async fn new() -> Self {
        let cx = Cx::current().unwrap();
        let mut link = attachment::Link::new(&cx).await;
        let input = link.attach(&cx, 9, MediaRole::Input).await;
        let (h, c) = link.attach(&cx, 10, MediaRole::Clipboard).await;
        let grant = authority::grant(&cx);
        let host = authority::host(grant);
        let scope = Binding {
            session: grant.request.parent.remote_session,
            lease: grant.lease,
        };
        let lane = ClipboardChannel::new(&link.h, h, scope).unwrap();
        let peer = ClipboardChannel::new(&link.c, c, scope).unwrap();
        let (bridge, seed) = Bridge::host(cx.clone(), &link.h, lane, &host, true).unwrap();
        Self {
            cx,
            link,
            bridge,
            seed,
            peer,
            grant,
            _host: host,
            _input: input,
        }
    }
    fn offer(&mut self, id: u128) {
        channel(&mut self.seed)
            .offer(
                id,
                "current selection",
                None,
                HostInstant::from_micros(clock(&self.cx)),
            )
            .unwrap();
    }
    fn pump(&mut self) -> Pump {
        let WorkerSeed {
            session,
            shared,
            clock: timer,
            cx,
        } = &mut self.seed;
        let Some(worker::Session::Host(channel)) = session else {
            panic!("host owner")
        };
        let mut scratch = vec![0; shared.maximum];
        let mut sink = shared::Sink {
            shared,
            clock: timer,
            cx,
        };
        channel
            .pump(&mut scratch, &mut sink, || {
                HostInstant::from_micros(clock(cx))
            })
            .unwrap()
    }
    fn hold_pending(&mut self) {
        assert!(self.bridge.pending.is_none());
        let mut outbox = self.bridge.shared.outbox.lock().unwrap();
        assert!(outbox.busy);
        self.bridge.pending = Some(outbox.item.take().unwrap());
    }
    fn stop(&mut self) {
        self.bridge.retire(&mut self.link.h).unwrap();
        self.peer.retire(&mut self.link.c, &self.cx).unwrap();
    }
}
fn channel(seed: &mut WorkerSeed) -> &mut ChannelSession {
    match seed.session.as_mut().unwrap() {
        worker::Session::Host(channel) => channel,
        worker::Session::Controller(_) => panic!("host owner"),
    }
}

#[test]
fn an_obsolete_unsent_handoff_does_not_request_transport_retirement() {
    runtime().block_on(async {
        let mut f = Fixture::new().await;
        f.offer(101);
        assert_eq!(f.pump(), Pump::RecordAccepted);
        f.hold_pending(); // The exact one-slot state retained across backpressure.
        let old = decode(
            &f.bridge.pending.as_ref().unwrap().bytes.0,
            f.bridge.lane.outgoing(),
            &f.bridge.lane.limits(),
        )
        .unwrap()
        .stamp;
        f.offer(102);
        assert!(f.bridge.permits_io());
        f.link.h.tick(&f.cx, || true).unwrap();
        f.bridge.lane.check(&f.link.h).unwrap();
        assert_eq!(f.bridge.maintain(&mut f.link.h), Ok(State::Active));
        assert!(f.bridge.pending.is_none());
        assert!(!f.bridge.shared.outbox.lock().unwrap().busy);
        assert!(f.bridge.inflight.iter().all(Option::is_none));
        assert_eq!(f.pump(), Pump::CancelAccepted(old));
        {
            let outbox = f.bridge.shared.outbox.lock().unwrap();
            assert_eq!(
                decode(
                    &outbox.item.as_ref().unwrap().bytes.0,
                    f.bridge.lane.outgoing(),
                    &f.bridge.lane.limits(),
                )
                .unwrap()
                .body,
                Body::Cancel(CancelReason::Superseded)
            );
        }
        f.stop();
    });
}

#[test]
fn the_immediate_record_check_refuses_a_superseded_unsent_begin() {
    runtime().block_on(async {
        let mut f = Fixture::new().await;
        f.offer(201);
        assert_eq!(f.pump(), Pump::RecordAccepted);
        let original = f.link.h.usage().retained_send_records;
        let mut calls = 0;
        let sender = channel(&mut f.seed);
        let result = f.bridge.service(&mut f.link.h, || {
            calls += 1;
            if calls == 2 {
                // First connection check is tick; this one is send_prepared,
                // after service's ordinary admission and before exact enqueue.
                sender
                    .offer(
                        202,
                        "changed at admission",
                        None,
                        HostInstant::from_micros(clock(&f.cx)),
                    )
                    .unwrap();
            }
            true
        });
        assert_eq!(result, Ok(State::Active));
        assert!(calls >= 2);
        assert_eq!(f.link.h.usage().retained_send_records, original);
        assert!(f.bridge.lane.send_drained(&f.link.h).unwrap());
        assert!(f.bridge.pending.is_none());
        assert!(f.bridge.inflight.iter().all(Option::is_none));
        assert!(!f.bridge.shared.outbox.lock().unwrap().busy);
        assert!(matches!(f.pump(), Pump::CancelAccepted(_)));
        f.stop();
    });
}

#[test]
fn an_exact_drained_witness_releases_obsolete_egress_before_maintenance() {
    runtime().block_on(async {
        let mut f = Fixture::new().await;
        f.offer(301);
        assert_eq!(f.pump(), Pump::RecordAccepted);
        assert_eq!(f.bridge.service(&mut f.link.h, || true), Ok(State::Active));
        assert!(f.bridge.inflight.iter().any(Option::is_some));
        assert!(!f.bridge.lane.send_drained(&f.link.h).unwrap());
        let until = clock(&f.cx) + 1_000_000;
        while !f.bridge.lane.send_drained(&f.link.h).unwrap() {
            assert!(clock(&f.cx) < until, "native send never drained");
            f.link.drive(&f.cx).await;
            f.peer
                .dispatch(&mut f.link.c, &f.cx, || true, |_| Ok(Disposition::Consumed))
                .unwrap();
        }
        // Deliberately retain the bridge ledger until the next maintenance
        // turn, just as the containing connection driver does between polls.
        assert!(f.bridge.inflight.iter().any(Option::is_some));
        f.offer(302);
        assert_eq!(f.bridge.maintain(&mut f.link.h), Ok(State::Active));
        assert!(f.bridge.inflight.iter().all(Option::is_none));
        assert!(f.bridge.shared.gate.is_open());
        assert!(!f.link.h.is_closed());
        f.stop();
    });
}

#[test]
fn a_superseded_record_retained_by_quic_still_retires_the_lane() {
    runtime().block_on(async {
        let mut f = Fixture::new().await;
        f.offer(401);
        assert_eq!(f.pump(), Pump::RecordAccepted);
        assert_eq!(f.bridge.service(&mut f.link.h, || true), Ok(State::Active));
        assert!(!f.bridge.lane.send_drained(&f.link.h).unwrap());
        f.offer(402);
        assert_eq!(f.bridge.maintain(&mut f.link.h), Ok(State::Retired));
        assert_eq!(
            f.bridge.reason(),
            Some(Error::Session(SessionError::Clipboard(
                fr_core::clipboard::Error::LocalChanged
            )))
        );
        assert!(!f.link.h.is_closed());
        assert!(!f.bridge.shared.gate.is_open());
        f.stop();
    });
}

#[test]
fn an_already_retired_transport_lane_remains_an_optional_failure() {
    runtime().block_on(async {
        let mut f = Fixture::new().await;
        f.offer(501);
        assert_eq!(f.pump(), Pump::RecordAccepted);
        assert_eq!(f.bridge.service(&mut f.link.h, || true), Ok(State::Active));
        f.bridge.lane.request_retirement();
        f.link.h.tick(&f.cx, || true).unwrap();
        assert_eq!(f.bridge.maintain(&mut f.link.h), Ok(State::Retired));
        assert_eq!(f.bridge.reason(), Some(Error::Closed));
        assert!(!f.link.h.is_closed());
        assert_eq!(f.bridge.maintain(&mut f.link.h), Ok(State::Retired));
        f.stop();
    });
}

#[test]
fn a_controller_clock_expiry_cannot_discard_a_live_wire_sender() {
    runtime().block_on(async {
        let mut f = Fixture::new().await;
        let mut viewer = authority::viewer(&f.cx, f.grant);
        let mut owner = viewer
            .attach_clipboard_lane(
                f.peer.parent(),
                f.peer.outgoing(),
                f.peer.limits(),
                true,
                ClientInstant(clock(&f.cx)),
            )
            .unwrap();
        let transport = owner.transport();
        let shared = Shared {
            gate: transport.transport(),
            maximum: f.peer.limits().max_control_message_bytes() as usize,
            inbox: std::sync::Mutex::default(),
            outbox: std::sync::Mutex::default(),
            inbound_until: std::sync::atomic::AtomicU64::default(),
            wake: std::sync::OnceLock::default(),
        };
        let timer = Clock::Controller(transport);
        owner
            .offer(601, "controller item", None, ClientInstant(clock(&f.cx)))
            .unwrap();
        let mut sink = shared::Sink {
            shared: &shared,
            clock: &timer,
            cx: &f.cx,
        };
        let mut scratch = vec![0; shared.maximum];
        assert_eq!(
            owner.pump(&mut scratch, &mut sink, || ClientInstant(clock(&f.cx))),
            Ok(Pump::RecordAccepted)
        );
        let record = shared.outbox.lock().unwrap().item.take().unwrap();
        let expired = Error::Session(
            record
                .permit
                .check_operation(record.permit.deadline())
                .unwrap_err(),
        );
        assert!(!record.can_discard(expired, &timer, &ProtocolLimits::ABSOLUTE));
        assert!(!record.can_discard(Error::HandoffExpired, &timer, &ProtocolLimits::ABSOLUTE));
        owner
            .offer(
                602,
                "controller replacement",
                None,
                ClientInstant(clock(&f.cx)),
            )
            .unwrap();
        let changed = Error::Session(
            record
                .permit
                .check_operation(HostInstant::from_micros(clock(&f.cx)))
                .unwrap_err(),
        );
        assert_eq!(
            changed,
            Error::Session(SessionError::Clipboard(
                fr_core::clipboard::Error::LocalChanged
            ))
        );
        assert!(record.can_discard(changed, &timer, &ProtocolLimits::ABSOLUTE));
        f.stop();
    });
}
