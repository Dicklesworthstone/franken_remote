//! Actual TLS/UDP and original audio source service. Observation policy and
//! compressed source bytes are explicit fixtures, not native codec evidence.
use super::*;
use crate::test_support::transport_support;
use asupersync::cx::Cx;
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    time::HostDuration,
};
use fr_transport::quic::{MediaChannel, Route};
use fr_wire::{
    attachment::MediaRole,
    negotiation::{Capability, Role},
};
use std::time::Duration;
use transport_support::{clock, runtime};

// The microphone tests' original UDP/TLS link, compiled once there.
use crate::microphone_quic::tests::attachment as network;

async fn fixture(
    cx: &Cx,
) -> (
    network::Link,
    AudioFeed,
    AudioLanes,
    (MediaChannel, MediaChannel),
) {
    fixture_with_lifetime(cx, HostDuration::from_micros(10_000_000)).await
}

async fn fixture_with_lifetime(
    cx: &Cx,
    authorization_lifetime: HostDuration,
) -> (
    network::Link,
    AudioFeed,
    AudioLanes,
    (MediaChannel, MediaChannel),
) {
    let mut link = network::Link::new(cx).await;
    link.selection.role = Role::Observe;
    link.selection.capabilities.push(Capability {
        name: wire::CAPABILITY.into(),
        version: 1,
        required: true,
    });
    link.selection.capabilities.push(Capability {
        name: fr_wire::attachment::DELIVERY_CAPABILITY.into(),
        version: fr_wire::attachment::DELIVERY_VERSION,
        required: true,
    });
    link.selection
        .capabilities
        .sort_by(|a, b| a.name.cmp(&b.name));
    let pair = link.attach(cx, 10, MediaRole::AudioDown).await;
    let host = pair.0.completed_on(&link.h).unwrap();
    let client = pair.1.completed_on(&link.c).unwrap();
    let host_lanes = AudioLanes {
        control: host.outbound,
        replies: host.inbound,
        packets: host.datagram.unwrap(),
        binding: 10,
        packet_maximum: usize::try_from(host.byte_allowance).unwrap(),
    };
    let client_lanes = AudioLanes {
        control: client.inbound,
        replies: client.outbound,
        packets: client.datagram.unwrap(),
        binding: 10,
        packet_maximum: usize::try_from(client.byte_allowance).unwrap(),
    };
    let mut authority = SessionAuthority::new(
        network::parent().remote_session,
        AuthorityPolicy {
            authorization_lifetime,
            ticket_lifetime: HostDuration::from_micros(
                authorization_lifetime.as_micros().min(1_000_000),
            ),
        },
    );
    authority.mark_capabilities_checked().unwrap();
    authority
        .authorize_observation(crate::media::host_now(cx).unwrap())
        .unwrap();
    let owner = ObservationControl::new(cx.clone(), authority).unwrap();
    // The exact registered attachment and connection provide the state that
    // the public exclusive constructor ordinarily takes from NegotiatedMedia.
    let feed = AudioFeed {
        target: FeedTarget::Exclusive(Arc::new(Mutex::new(State {
            owner,
            ring: ring(),
            audio: EntryAudio {
                lanes: Some(host_lanes),
                ..EntryAudio::default()
            },
            closed: false,
            connection: link.h.binding(),
            lanes: host_lanes,
        }))),
    };
    let until = clock(cx) + 1_000_000;
    while link.h.usage().retained_send_records != 0 || link.c.usage().retained_send_records != 0 {
        assert!(clock(cx) < until);
        link.drive(cx).await;
    }
    (link, feed, client_lanes, pair)
}

#[test]
fn expired_configuration_send_stops_source_demand_without_ending_observation() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (mut link, feed, _lanes, _pair) = Box::pin(fixture(&cx)).await;
        let original = link.h.binding();
        assert_eq!(feed.service_exclusive(&mut link.h).unwrap().accepted, 1);
        assert_eq!(link.h.usage().retained_send_records, 1);
        // Leave the actual admitted configuration undriven. No transport ACK
        // or output-readiness evidence exists for it.
        asupersync::time::sleep(cx.now(), Duration::from_millis(2100)).await;
        let report = feed.service_exclusive(&mut link.h).unwrap();
        assert_eq!(report.accepted, 0);
        assert!(!report.pending);
        assert!(!feed.demand().unwrap());
        let FeedTarget::Exclusive(shared) = &feed.target else {
            unreachable!()
        };
        {
            let state = shared.lock().unwrap();
            assert!(state.closed && state.ring.is_empty());
            assert!(state.audio.lane.is_stopped());
            assert!(state.audio.control_offer.is_none());
            state.owner.check().unwrap();
            assert_eq!(link.h.audio_send_expired(state.lanes.replies), Ok(true));
            assert_eq!(link.h.audio_receive_expired(state.lanes.replies), Ok(false));
        }
        assert_eq!(link.h.usage().retained_send_records, 0);
        assert!(link.h.is_bound_to(&original));
        assert!(!link.h.is_closed());
        assert_eq!(feed.service_exclusive(&mut link.h).unwrap().accepted, 0);
    });
}

#[test]
fn expired_reply_fences_source_demand_and_ring_before_an_idle_turn() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (mut link, feed, lanes, _pair) = Box::pin(fixture(&cx)).await;
        let original = link.h.binding();
        assert!(feed.demand().unwrap());
        assert_eq!(feed.service_exclusive(&mut link.h).unwrap().accepted, 1);
        let until = clock(&cx) + 1_000_000;
        let mut configured = false;
        while !configured || link.h.usage().retained_send_records != 0 {
            assert!(clock(&cx) < until);
            link.drive(&cx).await;
            link.c
                .receive_ready(
                    &cx,
                    || true,
                    |route| route == Route::Stream(lanes.control),
                    |_, bytes| {
                        let offer = wire::decode_configuration(bytes, lanes.binding).unwrap();
                        assert_eq!(offer.generation.as_raw(), 1);
                        configured = true;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }
        let reply = acknowledgement(1, lanes.binding);
        link.c
            .send(
                &cx,
                Route::Stream(lanes.replies),
                &reply,
                clock(&cx) + 4_000_000,
                || true,
            )
            .unwrap();
        let until = clock(&cx) + 1_000_000;
        let mut held = false;
        while !held || link.c.usage().retained_send_records != 0 {
            assert!(clock(&cx) < until);
            link.drive(&cx).await;
            link.h
                .receive_ready(
                    &cx,
                    || true,
                    |route| matches!(route, Route::Stream(r) if r.binding == lanes.binding),
                    |_, bytes| {
                        assert_eq!(bytes, reply);
                        held = true;
                        Ok(Disposition::Blocked)
                    },
                )
                .unwrap();
        }
        let FeedTarget::Exclusive(shared) = &feed.target else {
            unreachable!()
        };
        {
            let mut state = shared.lock().unwrap();
            push(&mut state.ring, 0, clock(&cx));
            assert_eq!(state.ring.len(), 1);
        }
        asupersync::time::sleep(cx.now(), Duration::from_millis(2100)).await;
        // Neither parent drive nor a send has serviced expiry yet. Source
        // service itself must close demand even when its lane has no packet.
        assert!(feed.demand().unwrap());
        let report = feed.service_exclusive(&mut link.h).unwrap();
        assert_eq!(report.accepted, 0);
        assert!(!report.pending);
        assert!(!feed.demand().unwrap());
        {
            let state = shared.lock().unwrap();
            assert!(state.closed && state.ring.is_empty());
            assert!(state.audio.lane.is_stopped());
            assert!(state.audio.control_offer.is_none());
            state.owner.check().unwrap();
            assert_eq!(link.h.audio_receive_expired(state.lanes.replies), Ok(true));
        }
        assert!(link.h.is_bound_to(&original));
        assert!(!link.h.is_closed());
        assert_eq!(feed.service_exclusive(&mut link.h).unwrap().accepted, 0);
    });
}

async fn deliver_reply(
    cx: &Cx,
    link: &mut network::Link,
    feed: &AudioFeed,
    lanes: AudioLanes,
    bytes: &[u8],
) {
    let FeedTarget::Exclusive(shared) = &feed.target else {
        unreachable!()
    };
    let inbound = shared.lock().unwrap().lanes.replies;
    link.c
        .send(
            cx,
            Route::Stream(lanes.replies),
            bytes,
            clock(cx) + 1_000_000,
            || true,
        )
        .unwrap();
    let until = clock(cx) + 1_000_000;
    let mut received = false;
    while !received || link.c.usage().retained_send_records != 0 {
        assert!(clock(cx) < until, "original audio reply was not drained");
        link.drive(cx).await;
        link.h
            .receive_ready(
                cx,
                || true,
                |route| route == Route::Stream(inbound),
                |route, record| {
                    assert_eq!(record, bytes);
                    received = true;
                    // The production session dispatcher requires its service
                    // to own the record; None is a parent handler failure.
                    feed.receive_exclusive(route, record)
                        .map_err(|_| ())?
                        .ok_or(())
                },
            )
            .unwrap();
    }
}

#[test]
fn rejected_audio_keeps_late_original_replies_out_of_the_parent_failure_path() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (mut link, feed, lanes, _pair) = Box::pin(fixture(&cx)).await;
        let original = link.h.binding();
        assert_eq!(feed.service_exclusive(&mut link.h).unwrap().accepted, 1);
        let mut configured = false;
        let until = clock(&cx) + 1_000_000;
        while !configured || link.h.usage().retained_send_records != 0 {
            assert!(clock(&cx) < until);
            link.drive(&cx).await;
            link.c
                .receive_ready(
                    &cx,
                    || true,
                    |route| route == Route::Stream(lanes.control),
                    |_, bytes| {
                        let offer = wire::decode_configuration(bytes, lanes.binding).unwrap();
                        assert_eq!(offer.generation.as_raw(), 1);
                        configured = true;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }

        let mut rejected = acknowledgement(1, lanes.binding);
        let mut refusal = wire::decode_configured(&rejected, lanes.binding).unwrap();
        refusal.accepted = false;
        wire::encode_configured(&refusal, lanes.binding, &mut rejected).unwrap();
        deliver_reply(&cx, &mut link, &feed, lanes, &rejected).await;
        assert_eq!(feed.service_exclusive(&mut link.h).unwrap().accepted, 0);
        assert!(!feed.demand().unwrap());

        let mut stop = vec![0; wire::AUDIO_STOP_RECORD_BYTES];
        wire::encode_stop(
            &wire::AudioStop {
                direction: AudioDirection::Downlink,
                generation: AudioGeneration::from_raw(1),
                reason: AudioStopReason::DeviceChanged,
            },
            lanes.binding,
            &mut stop,
        )
        .unwrap();
        // Output cleanup and configuration replies may already be queued on
        // this same reliable lane. Neither a restart request nor a matching or
        // newer acknowledgement can revive the rejected optional source.
        for late in [
            stop,
            acknowledgement(1, lanes.binding),
            acknowledgement(2, lanes.binding),
        ] {
            deliver_reply(&cx, &mut link, &feed, lanes, &late).await;
            let report = feed.service_exclusive(&mut link.h).unwrap();
            assert_eq!(report.accepted, 0);
            assert!(!report.pending);
            assert!(!feed.demand().unwrap());
            let FeedTarget::Exclusive(shared) = &feed.target else {
                unreachable!()
            };
            let state = shared.lock().unwrap();
            assert!(state.closed && state.ring.is_empty());
            assert!(state.audio.lane.is_stopped());
            assert!(state.audio.control_offer.is_none());
            assert_eq!(state.audio.lane.counters().configurations, 1);
            assert_eq!(state.audio.lane.counters().restarts, 0);
            state.owner.check().unwrap();
            assert!(link.h.has_route(Route::Stream(state.lanes.replies)));
            assert!(link.h.has_route(Route::Stream(link.hr.inbound)));
            assert!(link.h.has_route(Route::Stream(link.hr.outbound)));
            assert!(link.h.is_bound_to(&original));
            assert!(!link.h.is_closed());
            assert_eq!(link.h.usage().retained_send_records, 0);
        }
    });
}

#[test]
fn stopped_audio_consumes_only_its_exact_original_reply_route_until_parent_revoke() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (link, feed, lanes, _pair) = Box::pin(fixture(&cx)).await;
        let FeedTarget::Exclusive(shared) = &feed.target else {
            unreachable!()
        };
        let (owner, original, packets) = {
            let state = shared.lock().unwrap();
            (state.owner.clone(), state.lanes.replies, state.lanes.packets)
        };
        feed.close_exclusive();
        let bytes = acknowledgement(1, lanes.binding);
        assert_eq!(
            feed.receive_exclusive(Route::Stream(original), &bytes),
            Ok(Some(Disposition::Consumed))
        );
        let mut foreign = [original; 6];
        foreign[0].stream = link.hr.inbound.stream;
        foreign[1].binding += 1;
        foreign[2].outbound = true;
        foreign[3].messages = Messages::AudioControl;
        foreign[4].priority = Priority::Bulk;
        foreign[5].maximum += 1;
        for route in foreign {
            assert_eq!(
                feed.receive_exclusive(Route::Stream(route), &bytes),
                Ok(None)
            );
        }
        for route in [Route::Stream(link.hr.inbound), Route::Datagram(packets)] {
            assert_eq!(feed.receive_exclusive(route, &bytes), Ok(None));
        }
        owner.check().unwrap();
        owner.revoke();
        assert!(matches!(
            feed.receive_exclusive(Route::Stream(original), &bytes),
            Err(Error::Media(_))
        ));
        assert!(!feed.demand().unwrap());
    });
}

#[test]
fn stopped_audio_cannot_consume_replies_after_its_original_observation_deadline() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (_link, feed, lanes, _pair) = Box::pin(fixture_with_lifetime(
            &cx,
            HostDuration::from_micros(100_000),
        ))
        .await;
        let FeedTarget::Exclusive(shared) = &feed.target else {
            unreachable!()
        };
        let original = {
            let state = shared.lock().unwrap();
            state.lanes.replies
        };
        feed.close_exclusive();
        // The original authority was created before this wait. Even a delayed
        // fixture return cannot give that observation a fresh lifetime.
        asupersync::time::sleep(cx.now(), Duration::from_millis(100)).await;
        assert!(matches!(
            feed.receive_exclusive(Route::Stream(original), &acknowledgement(1, lanes.binding)),
            Err(Error::Media(crate::media::Error::Authority(
                fr_core::authority::AuthorityError::ObservationExpired
            )))
        ));
        assert!(!feed.demand().unwrap());
    });
}
