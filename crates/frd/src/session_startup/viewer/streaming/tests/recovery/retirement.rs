//! Real authenticated streams retire during the ORIGINAL viewer network turn.
//! Decode borrowing and output are explicit fixtures, never native/HEVC claims.
use super::*;
use crate::session_startup::{ControlledHost, viewer::controlled};
use fr_client::input::Action;
use fr_core::{ids::InputTicketId, input::PhysicalKey};
use fr_transport::quic::ControlRoutes;
use fr_wire::{attachment::Ticket, negotiation::ControlBinding};
use std::{cell::Cell, future::poll_fn, pin::pin, task::Poll};

fn retire(
    host: &mut ControlledHost,
    media: NegotiatedMedia,
    h: &Cx,
    parent: ControlBinding,
    until: u64,
) -> crate::media_quic::replacement::Replacement {
    let (q, routes) = host.io().unwrap();
    let mut replacement = media
        .begin_replacement(
            h,
            q,
            routes,
            parent,
            until,
            Some([Ticket(41_001), Ticket(41_002), Ticket(41_003)]),
            || true,
        )
        .unwrap();
    assert!(!replacement.advance(h, q, || true).unwrap());
    replacement
}

fn borrow_picture(
    receiver: &mut ReceivePipeline,
    media: &NegotiatedMedia,
    c: &Cx,
) -> fr_media::delivery::ReceivedPicture {
    let descriptor = FrameDescriptor {
        frame: 1,
        reference: Some(0),
        total_bytes: 4,
        stride: 4,
        capture_micros: now(c).unwrap(),
    };
    let mut bytes = [0; 1150];
    let n = encode_fragment(
        Fragment {
            descriptor,
            index: 0,
            bytes: b"held",
        },
        media.bindings().for_channel(Channel::Video),
        &media.limits(),
        &mut bytes,
    )
    .unwrap();
    receiver
        .receive(Channel::Video, &bytes[..n], now(c).unwrap())
        .unwrap();
    receiver.take_decodable(now(c).unwrap()).unwrap().unwrap()
}

fn fail_reference(receiver: &mut ReceivePipeline, media: &NegotiatedMedia, c: &Cx) {
    let mut bytes = [0; 1150];
    let n = encode_progress(
        Progress {
            descriptor: FrameDescriptor {
                frame: 1,
                reference: Some(0),
                total_bytes: 4,
                stride: 4,
                capture_micros: now(c).unwrap(),
            },
            observed_micros: now(c).unwrap(),
            observation: SourceObservation::Captured,
            pipeline: PipelineState::Failed,
        },
        media.bindings().for_channel(Channel::MediaConfig),
        &media.limits(),
        &mut bytes,
    )
    .unwrap();
    assert_eq!(
        receiver.receive(Channel::MediaConfig, &bytes[..n], now(c).unwrap()),
        Err(DeliveryError::ReferenceExpired)
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn peer_retirement_during_controlled_decode_drain_preserves_original_recovery() {
    run(|c, h| async move {
        let (mut host, viewer, host_media, mut receiver, driver) =
            Box::pin(controlled::tests::recovery_fixture(&c, &h)).await;
        let original_grant = viewer.input_result_binding();
        let stop = StreamingViewerControl {
            cx: c.clone(),
            input: Some(viewer.control()),
        };
        let mut peer = Peer::Control(Box::new(viewer));
        let (session, media) = peer.parts().unwrap();
        let parent = session.opened.binding;
        let connection = session.transport.binding();
        let old_routes = media.retiring_viewer_routes(&session.transport).unwrap();
        let mut binding = media.binding();
        binding.parent = parent;
        let mut watcher = media
            .recovery_receiver(&session.transport, session.routes, parent, &receiver)
            .unwrap();
        let picture = borrow_picture(&mut receiver, media, &c);
        fail_reference(&mut receiver, media, &c);
        let charged = receiver.budget_usage();
        assert_ne!(charged, BudgetUsage::default());

        let release = Cell::new(false);
        let mut native = pin!(async {
            poll_fn(|_| {
                if release.get() {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            Ok::<_, media::Error>(picture)
        });
        let ControlRoutes { inbound, .. } = host.io().unwrap().1;
        let request_seen = Cell::new(false);
        let mut original_deadline = None;
        let mut media = Some(host_media);
        let mut replacement = None;
        let mut tickets = 50_000;
        let mut retired_turns = 0;
        let until = now(&c).unwrap() + 1_000_000;
        let exercise = async {
            loop {
                assert!(now(&c).unwrap() < until, "retirement was not drained");
                if request_seen.get()
                    && let Some(media) = media.take()
                {
                    replacement = Some(retire(
                        &mut host,
                        media,
                        &h,
                        parent,
                        original_deadline.unwrap(),
                    ));
                }
                let (sent, drained) = Box::pin(support::both(
                    host.drive(
                        Duration::from_millis(1),
                        || Ok(crate::session_startup::tests::unique_nonce()),
                        || {
                            tickets += 1;
                            Some(InputTicketId::from_raw(tickets))
                        },
                        |route, bytes| {
                            assert_eq!(route, Route::Stream(inbound));
                            let request = recovery_request::decode(
                                bytes,
                                binding,
                                &ProtocolLimits::ABSOLUTE,
                                fr_wire::input::InputDirection::ViewerToHost,
                                fr_wire::input::InputDelivery::Reliable,
                            )
                            .unwrap();
                            assert_eq!(request.reason, recovery_request::Reason::ReferenceExpired);
                            assert!(!request_seen.replace(true), "recovery request replayed");
                            Ok(Disposition::Consumed)
                        },
                    ),
                    native_turn::drive(
                        native.as_mut(),
                        network_turn(
                            &mut peer,
                            &mut receiver,
                            &mut Repair::default(),
                            Some(&mut watcher),
                            &mut Statistics::default(),
                            None,
                            &c,
                            // The replacement offer belongs to the handoff,
                            // never to the embedding application's callback.
                            &mut |_, _| Err(()),
                        ),
                        &stop,
                    ),
                ))
                .await;
                sent.unwrap();
                let drained = drained.expect("peer reset must not end the original network turn");
                let deadline = watcher.next_deadline().unwrap();
                assert_eq!(*original_deadline.get_or_insert(deadline), deadline);
                let session = peer.parent().unwrap();
                assert!(session.transport.is_bound_to(&connection));
                if old_routes
                    .iter()
                    .all(|route| !session.transport.has_route(*route))
                {
                    retired_turns += 1;
                }
                let viewer = peer.controlled().unwrap();
                assert!(!viewer.is_closed());
                assert_eq!(viewer.input_result_binding(), original_grant);
                assert!(viewer.suspension_totals().0 > 0);
                assert!(
                    viewer
                        .action(Action::Key {
                            key: PhysicalKey::new(4).unwrap(),
                            transition: fr_core::input::KeyTransition::Press,
                        })
                        .is_err()
                );
                assert!(!viewer.is_closed(), "suspended input must retain the grant");
                assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
                assert_eq!(receiver.budget_usage(), charged);
                if let Some(picture) = drained {
                    assert!(
                        retired_turns >= 3,
                        "old media must retire before native release"
                    );
                    assert!(
                        !picture.is_live(),
                        "obsolete native work regained freshness"
                    );
                    drop(picture);
                    assert_eq!(receiver.budget_usage(), BudgetUsage::default());
                    let retained = viewer.take_media_for_recovery(None).unwrap();
                    assert_eq!(retained.binding().recovery, binding.recovery);
                    assert!(!viewer.is_closed());
                    break;
                }
                if retired_turns >= 3 {
                    release.set(true);
                }
            }
            assert!(replacement.is_some());
            peer.close();
            host.close();
            receiver.close();
            watcher.close();
        };
        let ((), summary) = Box::pin(support::both(exercise, driver)).await;
        assert!(summary.handoff_safe());
    });
}

#[test]
fn unsolicited_media_retirement_still_ends_a_healthy_controlled_viewer() {
    run(|c, h| async move {
        let (mut host, viewer, media, mut receiver, driver) =
            Box::pin(controlled::tests::recovery_fixture(&c, &h)).await;
        let mut peer = Peer::Control(Box::new(viewer));
        let (session, viewer_media) = peer.parts().unwrap();
        let parent = session.opened.binding;
        let mut watcher = viewer_media
            .recovery_receiver(&session.transport, session.routes, parent, &receiver)
            .unwrap();
        let until = now(&c).unwrap() + 1_000_000;
        let _replacement = retire(&mut host, media, &h, parent, until);
        let exercise = async {
            loop {
                assert!(now(&c).unwrap() < until, "unsolicited reset was ignored");
                let (sent, delivery) = Box::pin(support::both(
                    host.io()
                        .unwrap()
                        .0
                        .drive(&h, Duration::from_millis(1), || true),
                    network_turn(
                        &mut peer,
                        &mut receiver,
                        &mut Repair::default(),
                        Some(&mut watcher),
                        &mut Statistics::default(),
                        None,
                        &c,
                        &mut |_, _| Err(()),
                    ),
                ))
                .await;
                if let Err(error) = delivery {
                    // Input records name the media configuration channel, so
                    // a healthy grant checks that channel too, before the
                    // media check runs: its unsolicited reset ends the grant
                    // as closed input. Only a recovering grant skips it.
                    assert_eq!(
                        error,
                        Error::Control(controlled::Error::Input(crate::input_quic::Error::Closed))
                    );
                    break;
                }
                sent.unwrap();
            }
            assert!(peer.controlled().unwrap().is_closed());
            assert_eq!(watcher.state(), recovery_control::State::Receiving);
            peer.close();
            host.close();
            receiver.close();
            watcher.close();
        };
        let ((), summary) = Box::pin(support::both(exercise, driver)).await;
        assert!(summary.handoff_safe());
    });
}

#[test]
#[allow(clippy::too_many_lines)]
fn recovery_network_turn_retires_old_queries_and_holds_the_next_feedback_owner() {
    run(|c, h| async move {
        let (mut host, mut peer, mut receiver, mut watcher, _) = Box::pin(pair(&c, &h)).await;
        let (_, mut feedback) = telemetry(&mut host, &mut peer);
        let (session, media) = peer.parts().unwrap();
        let inbound = Route::Stream(session.routes.inbound);
        let mut old = media.binding();
        old.parent = session.opened.binding;
        let mut next = old;
        next.recovery = next.recovery.next().unwrap();
        let query = |binding, sequence| {
            let mut bytes = vec![0; receiver_metrics::QUERY_BYTES];
            receiver_metrics::encode(
                receiver_metrics::Message::Query { sequence },
                binding,
                &ProtocolLimits::ABSOLUTE,
                &mut bytes,
                fr_wire::input::InputDirection::HostToViewer,
                fr_wire::input::InputDelivery::Reliable,
            )
            .unwrap();
            bytes
        };
        // A previously prepared but unadmitted reply is owned by this old
        // responder. Retirement must suppress it even while that owner lives.
        feedback
            .receive(inbound, &query(old, 1), &receiver, now(&c).unwrap())
            .unwrap();
        fail_reference(&mut receiver, media, &c);
        let queries = [query(old, 2), query(next, 1)];
        let outbound = Route::Stream(host.io().unwrap().1.outbound);
        let started = now(&c).unwrap();
        let mut queued = 0;
        let mut held_turns = 0;
        let mut requests = 0;
        let mut deadline = None;
        while held_turns < 3 {
            assert!(now(&c).unwrap() < started + 1_000_000);
            if let Some(bytes) = queries.get(queued)
                && announce(&h, &mut host, outbound, bytes, started)
            {
                queued += 1;
            }
            let (sent, delivery) = Box::pin(support::both(
                host.drive(
                    Duration::from_millis(1),
                    || Ok(crate::session_startup::tests::unique_nonce()),
                    |_, bytes| {
                        // No old telemetry reply may be admitted, including
                        // the one prepared before the chain failed.
                        recovery_request::decode(
                            bytes,
                            old,
                            &ProtocolLimits::ABSOLUTE,
                            fr_wire::input::InputDirection::ViewerToHost,
                            fr_wire::input::InputDelivery::Reliable,
                        )
                        .unwrap();
                        requests += 1;
                        Ok(Disposition::Consumed)
                    },
                ),
                network_turn(
                    &mut peer,
                    &mut receiver,
                    &mut Repair::default(),
                    Some(&mut watcher),
                    &mut Statistics::default(),
                    Some(&mut feedback),
                    &c,
                    &mut |_, _| Err(()),
                ),
            ))
            .await;
            sent.unwrap();
            delivery.unwrap();
            assert_eq!(feedback.sent, 0);
            let until = watcher.next_deadline().unwrap();
            assert_eq!(*deadline.get_or_insert(until), until);
            let session = peer.parent().unwrap();
            let mut held = false;
            session
                .transport
                .receive_ready(
                    &c,
                    || true,
                    |route| route == inbound,
                    |_, bytes| {
                        if feedback::is_feedback(bytes) {
                            assert_eq!(bytes, queries[1]);
                            held = true;
                        }
                        Ok(Disposition::Blocked)
                    },
                )
                .unwrap();
            if held {
                held_turns += 1;
            }
        }
        assert_eq!(queued, 2);
        assert_eq!(requests, 1);
        assert_eq!(watcher.state(), recovery_control::State::Requested);
        host.close();
        peer.close();
        receiver.close();
        watcher.close();
    });
}
