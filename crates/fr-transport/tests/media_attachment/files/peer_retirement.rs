//! The viewer may finish Attached before the host promotes its original pair.
use super::*;

fn pending_host_routes(channel: &MediaChannel) -> ControlRoutes {
    let descriptor = channel.descriptor();
    let outbound = StreamRoute {
        stream: StreamId(descriptor.host_stream),
        binding: descriptor.binding.parent.id,
        messages: Messages::Exact(0x1a),
        priority: Priority::Bulk,
        outbound: true,
        maximum: attachment::GRANT_RECORD_BYTES,
    };
    ControlRoutes {
        outbound,
        inbound: StreamRoute {
            stream: StreamId(descriptor.viewer_stream),
            messages: Messages::Exact(0x19),
            outbound: false,
            ..outbound
        },
    }
}

async fn viewer_finished_first(
    l: &mut Link,
    cx: &Cx,
) -> (MediaChannel, MediaChannel, AttachedChannel) {
    let mut host = offer(l, cx, 9, MediaRole::Files).unwrap();
    let mut viewer = l.viewer(cx, &mut host).await;
    let until = clock(cx) + 1_000_000;
    loop {
        assert!(clock(cx) < until, "viewer attachment did not complete");
        host.transmit(&mut l.h, cx, || true).unwrap();
        viewer.transmit(&mut l.c, cx, || true).unwrap();
        l.drive(cx).await;
        host.dispatch(&mut l.h, cx, || true).unwrap();
        viewer.dispatch(&mut l.c, cx, || true).unwrap();
        // Deliberately withhold host.finish. Receiving Attached proves that
        // the host authenticated Attach and staged the complete final reply.
        if let Some(routes) = viewer.finish(&mut l.c, cx, || true).unwrap() {
            assert!(!host.is_complete());
            assert!(
                l.h.send_staged(pending_host_routes(&host).outbound)
                    .unwrap()
            );
            return (host, viewer, routes);
        }
    }
}

async fn peer_reset(l: &mut Link, cx: &Cx, host: &MediaChannel) {
    let incoming = pending_host_routes(host).inbound;
    let until = clock(cx) + 1_000_000;
    while !l.h.receive_ended(incoming).unwrap() {
        assert!(clock(cx) < until, "host did not receive native Files reset");
        l.drive(cx).await;
    }
    assert!(!host.is_complete());
}

async fn cancelled_before_host_promotion(cx: &Cx, payload: bool) {
    let mut l = Link::new(cx).await;
    enable(&mut l);
    let (_host_input, _viewer_input, hi, ci) = attach(&mut l, cx, 8, MediaRole::Input).await;
    let connection = l.h.binding();
    let (mut host, viewer, _) = viewer_finished_first(&mut l, cx).await;
    assert_eq!(
        host.retire_files_if_peer_cancelled(&mut l.h, cx, || true),
        Ok(false)
    );
    let mut lane = FilesChannel::new(&l.c, viewer, InputLeaseId::from_raw(123), 456).unwrap();
    if payload {
        let bytes = encoded_file(&lane);
        lane.send(&mut l.c, cx, &bytes, clock(cx) + 1_000_000, || true)
            .unwrap();
        assert!(!lane.send_drained(&l.c).unwrap());
        let until = clock(cx) + 1_000_000;
        // Actual reliable-send retirement proves the host acknowledged these
        // bytes; no attachment owner or file parser runs on the host meanwhile.
        while !lane.send_drained(&l.c).unwrap() {
            assert!(clock(cx) < until, "file prefix did not drain");
            l.drive(cx).await;
        }
    }
    lane.retire(&mut l.c, cx).unwrap();
    peer_reset(&mut l, cx, &host).await;
    assert_eq!(
        host.retire_files_if_peer_cancelled(&mut l.h, cx, || true),
        Ok(true)
    );
    assert!(!host.is_complete());
    assert_eq!(host.completed_on(&l.h), Err(Error::WrongRoute));
    drop(host);
    l.h.tick(cx, || true).unwrap();
    l.c.tick(cx, || true).unwrap();
    assert!(l.h.is_bound_to(&connection));
    assert!(matches!(
        offer(&mut l, cx, 10, MediaRole::Files),
        Err(Error::WrongRoute)
    ));
    input::transfer(
        &mut l,
        cx,
        false,
        Route::Stream(ci.outbound),
        Route::Stream(hi.inbound),
        &record(0x40, 8, 128),
    )
    .await;
    let (outgoing, incoming) = (l.hr.outbound, l.cr.inbound);
    input::transfer(
        &mut l,
        cx,
        true,
        Route::Stream(outgoing),
        Route::Stream(incoming),
        &record(0x12, 7, 128),
    )
    .await;
}

#[test]
fn acknowledged_files_payload_survives_host_promotion_on_the_original_stream() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let (_host_input, _viewer_input, hi, ci) = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let connection = l.h.binding();
        let (mut host, viewer, viewer_routes) = viewer_finished_first(&mut l, &cx).await;
        let viewer = FilesChannel::new(&l.c, viewer, InputLeaseId::from_raw(123), 456).unwrap();
        let bytes = encoded_file(&viewer);
        let until = clock(&cx) + 1_000_000;
        viewer.send(&mut l.c, &cx, &bytes, until, || true).unwrap();
        assert!(!viewer.send_drained(&l.c).unwrap());
        while !viewer.send_drained(&l.c).unwrap() {
            assert!(clock(&cx) < until, "native file prefix did not drain");
            l.drive(&cx).await;
        }
        assert!(!host.is_complete());
        let routes = host.finish(&mut l.h, &cx, || true).unwrap().unwrap();
        assert_eq!(routes.inbound.messages, Messages::Files);
        assert_eq!(routes.descriptor, viewer_routes.descriptor);
        assert_eq!(routes.inbound.stream, viewer_routes.outbound.stream);
        let host = FilesChannel::new(&l.h, host, InputLeaseId::from_raw(123), 456).unwrap();
        let mut received = false;
        host.dispatch(
            &mut l.h,
            &cx,
            || true,
            |record| {
                assert!(!received);
                assert_eq!(record, bytes);
                received = true;
                Ok(Disposition::Consumed)
            },
        )
        .unwrap();
        assert!(received);
        assert!(l.h.is_bound_to(&connection));
        input::transfer(
            &mut l,
            &cx,
            false,
            Route::Stream(ci.outbound),
            Route::Stream(hi.inbound),
            &record(0x40, 8, 128),
        )
        .await;
        let (outgoing, incoming) = (l.hr.outbound, l.cr.inbound);
        input::transfer(
            &mut l,
            &cx,
            true,
            Route::Stream(outgoing),
            Route::Stream(incoming),
            &record(0x12, 7, 128),
        )
        .await;
    });
}

#[test]
fn peer_files_reset_after_attached_retires_before_host_promotion() {
    run_test!(cx, {
        cancelled_before_host_promotion(&cx, false).await;
    });
}

#[test]
fn peer_files_reset_discards_acknowledged_file_prefix_before_host_promotion() {
    run_test!(cx, {
        cancelled_before_host_promotion(&cx, true).await;
    });
}

#[test]
fn completed_files_peer_tombstone_does_not_construct_a_receiver_or_abandon_control() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let (_host_input, _viewer_input, hi, ci) = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (mut host, viewer, routes, _) = attach(&mut l, &cx, 9, MediaRole::Files).await;
        let mut lane = FilesChannel::new(&l.c, viewer, InputLeaseId::from_raw(123), 456).unwrap();
        lane.retire(&mut l.c, &cx).unwrap();
        let until = clock(&cx) + 1_000_000;
        while !l.h.receive_ended(routes.inbound).unwrap() {
            assert!(clock(&cx) < until, "completed Files pair never retired");
            l.drive(&cx).await;
        }
        assert!(!host.is_complete());
        assert_eq!(
            host.retire_files_if_peer_cancelled(&mut l.h, &cx, || true),
            Ok(true)
        );
        assert_eq!(host.completed_on(&l.h), Err(Error::WrongRoute));
        drop(host);
        l.h.tick(&cx, || true).unwrap();
        input::transfer(
            &mut l,
            &cx,
            false,
            Route::Stream(ci.outbound),
            Route::Stream(hi.inbound),
            &record(0x40, 8, 128),
        )
        .await;
        let (outgoing, incoming) = (l.hr.outbound, l.cr.inbound);
        input::transfer(
            &mut l,
            &cx,
            true,
            Route::Stream(outgoing),
            Route::Stream(incoming),
            &record(0x12, 7, 128),
        )
        .await;
    });
}

#[test]
fn final_files_retirement_rejects_foreign_and_client_owners_without_mutation() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let (mut input, _viewer_input, _, _) = attach(&mut l, &cx, 8, MediaRole::Input).await;
        assert_eq!(
            input.retire_files_if_peer_cancelled(&mut l.h, &cx, || true),
            Err(Error::WrongRoute)
        );
        assert!(input.is_complete());
        assert!(!l.h.is_closed());
        let (mut host, mut viewer, _) = viewer_finished_first(&mut l, &cx).await;
        assert_eq!(
            viewer.retire_files_if_peer_cancelled(&mut l.c, &cx, || true),
            Err(Error::WrongRoute)
        );
        assert!(viewer.is_complete());
        let mut lane = FilesChannel::new(&l.c, viewer, InputLeaseId::from_raw(123), 456).unwrap();
        lane.retire(&mut l.c, &cx).unwrap();
        peer_reset(&mut l, &cx, &host).await;
        let mut foreign = Link::new(&cx).await;
        let before = foreign.h.usage();
        assert_eq!(
            host.retire_files_if_peer_cancelled(&mut foreign.h, &cx, || true),
            Err(Error::WrongRoute)
        );
        assert_eq!(foreign.h.usage(), before);
        assert!(!foreign.h.is_closed());
        assert!(!l.h.is_closed());
        assert_eq!(
            host.retire_files_if_peer_cancelled(&mut l.h, &cx, || true),
            Ok(true)
        );
    });
}

#[test]
fn unfinished_files_offer_does_not_gain_a_retirement_escape() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let mut host = offer(&mut l, &cx, 9, MediaRole::Files).unwrap();
        let before = l.h.usage();
        assert_eq!(
            host.retire_files_if_peer_cancelled(&mut l.h, &cx, || true),
            Ok(false)
        );
        assert_eq!(l.h.usage(), before);
        assert_eq!(host.retire_files(&mut l.h, &cx), Err(Error::WrongRoute));
        assert!(host.transmit(&mut l.h, &cx, || true).unwrap());
        assert!(!l.h.is_closed());
        drop(host);
        assert!(l.h.tick(&cx, || true).is_err());
        assert!(l.h.is_closed());
    });
}

#[test]
fn parent_closure_and_authority_loss_win_over_a_valid_peer_files_reset() {
    run_test!(cx, {
        for close_parent in [false, true] {
            let mut l = Link::new(&cx).await;
            enable(&mut l);
            let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
            let (mut host, viewer, _) = viewer_finished_first(&mut l, &cx).await;
            let mut lane =
                FilesChannel::new(&l.c, viewer, InputLeaseId::from_raw(123), 456).unwrap();
            lane.retire(&mut l.c, &cx).unwrap();
            peer_reset(&mut l, &cx, &host).await;
            if close_parent {
                l.h.close();
            }
            assert_eq!(
                host.retire_files_if_peer_cancelled(&mut l.h, &cx, || close_parent),
                Err(if close_parent {
                    Error::Closed
                } else {
                    Error::Unauthorized
                })
            );
            assert!(!host.is_complete());
            assert!(l.h.is_closed());
        }
    });
}
