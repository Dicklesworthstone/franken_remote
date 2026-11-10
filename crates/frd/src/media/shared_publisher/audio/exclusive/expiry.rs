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

#[path = "../../../../microphone_quic/tests/network.rs"]
mod network;

async fn fixture(
    cx: &Cx,
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
            authorization_lifetime: HostDuration::from_micros(10_000_000),
            ticket_lifetime: HostDuration::from_micros(1_000_000),
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
