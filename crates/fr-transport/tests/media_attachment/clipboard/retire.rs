//! Real UDP/TLS optional-lane reset. Payloads are exact transport-codec fixtures;
//! no OS work, synthetic permission, or successful native publication is claimed.
use super::*;
use fr_core::clipboard::{Binding as Scope, Endpoint, Stamp};
use fr_transport::quic::clipboard::ClipboardChannel;
use fr_wire::clipboard::{self as wire, Body, Message};

fn scope() -> Scope {
    Scope {
        session: parent().remote_session,
        lease: InputLeaseId::from_raw(123),
    }
}
fn chunk(channel: &ClipboardChannel) -> Vec<u8> {
    let payload = vec![0x5a; 6000];
    let mut bytes = vec![0; 8192];
    let n = wire::encode(
        Message {
            stamp: Stamp {
                id: 1,
                source: if channel.outgoing().sender == wire::Role::Host {
                    Endpoint::Host
                } else {
                    Endpoint::Controller
                },
                sequence: 1,
            },
            body: Body::Chunk {
                index: 0,
                offset: 0,
                bytes: &payload,
            },
        },
        channel.outgoing(),
        &channel.limits(),
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(n);
    bytes
}
async fn quiet(l: &mut Link, cx: &Cx) {
    let until = clock(cx) + 1_000_000;
    while l.h.usage().retained_send_records != 0 || l.c.usage().retained_send_records != 0 {
        assert!(clock(cx) < until, "attachment control did not drain");
        l.drive(cx).await;
    }
}
async fn peer_retired(l: &mut Link, cx: &Cx, route: StreamRoute) {
    let until = clock(cx) + 1_000_000;
    while l.c.has_route(Route::Stream(route)) {
        assert!(
            clock(cx) < until,
            "peer did not process the optional-lane reset"
        );
        l.drive(cx).await;
    }
    assert!(!l.h.is_closed());
    assert!(!l.c.is_closed());
}

#[test]
fn retire_queued_clipboard_preserves_control_and_input_in_both_directions() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let (_hi, _ci, hi, ci) = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (hp, cp, ha, ca) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
        let mut h = ClipboardChannel::new(&l.h, hp, scope()).unwrap();
        let c = ClipboardChannel::new(&l.c, cp, scope()).unwrap();
        quiet(&mut l, &cx).await;
        let until = clock(&cx) + 1_000_000;
        h.send(&mut l.h, &cx, &chunk(&h), until, || true).unwrap();
        c.send(&mut l.c, &cx, &chunk(&c), until, || true).unwrap();
        let control = record(0x12, 7, 96);
        l.h.send(&cx, Route::Stream(l.hr.outbound), &control, until, || true)
            .unwrap();
        assert_eq!(l.h.usage().retained_send_records, 2);
        h.retire(&mut l.h, &cx).unwrap();
        assert_eq!(l.h.usage().retained_send_records, 1);
        assert_eq!(l.h.usage().critical_send_records, 1);
        assert!(!l.h.has_route(Route::Stream(ha.outbound)));
        peer_retired(&mut l, &cx, ca.outbound).await;
        let mut got = false;
        while !got {
            assert!(clock(&cx) < until);
            l.drive(&cx).await;
            l.c.receive_ready(
                &cx,
                || true,
                |r| r == Route::Stream(l.cr.inbound),
                |_, b| {
                    assert_eq!(b, control);
                    got = true;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        }
        let hr = l.hr;
        let cr = l.cr;
        for (host, send, receive, kind, binding) in [
            (true, hr.outbound, cr.inbound, 0x12, 7),
            (false, cr.outbound, hr.inbound, 0x12, 7),
            (true, hi.outbound, ci.inbound, 0x17, 8),
            (false, ci.outbound, hi.inbound, 0x40, 8),
        ] {
            input::transfer(
                &mut l,
                &cx,
                host,
                Route::Stream(send),
                Route::Stream(receive),
                &record(kind, binding, 96),
            )
            .await;
        }
        assert!(h.check(&l.h).is_err());
        assert!(c.check(&l.c).is_err());
        quiet(&mut l, &cx).await;
        assert_eq!(l.c.usage().retained_send_records, 0);
        h.retire(&mut l.h, &cx).unwrap();
        drop((h, c));
        l.h.tick(&cx, || true).unwrap();
        l.c.tick(&cx, || true).unwrap();
    });
}

#[test]
fn retire_discards_native_prefix_and_partial_receive_without_replaying_on_ack() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (hp, cp, ha, ca) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
        let mut h = ClipboardChannel::new(&l.h, hp, scope()).unwrap();
        let c = ClipboardChannel::new(&l.c, cp, scope()).unwrap();
        quiet(&mut l, &cx).await;
        h.send(&mut l.h, &cx, &chunk(&h), clock(&cx) + 1_000_000, || true)
            .unwrap();
        for _ in 0..3 {
            l.drive(&cx).await;
            c.dispatch(
                &mut l.c,
                &cx,
                || true,
                |_| panic!("record still has only a prefix"),
            )
            .unwrap();
        }
        assert!(l.c.usage().framed_capacity >= 6000);
        assert_eq!(l.h.usage().retained_send_records, 1);
        h.retire(&mut l.h, &cx).unwrap();
        peer_retired(&mut l, &cx, ca.inbound).await;
        assert_eq!(l.h.usage().retained_send_upper_bound, 0);
        assert_eq!(l.c.usage().remainder_bytes, 0);
        assert!(l.h.receive_ended(ha.inbound).unwrap());
        assert!(l.c.receive_finished(ca.inbound).unwrap());
        assert_eq!(
            l.h.send(
                &cx,
                Route::Stream(ha.outbound),
                &chunk(&h),
                clock(&cx) + 1_000_000,
                || true
            ),
            Err(Error::WrongRoute)
        );
        for _ in 0..12 {
            l.drive(&cx).await;
        }
        assert_eq!(l.h.usage().retained_send_records, 0);
        let send = l.hr.outbound;
        let receive = l.cr.inbound;
        input::transfer(
            &mut l,
            &cx,
            true,
            Route::Stream(send),
            Route::Stream(receive),
            &record(0x12, 7, 96),
        )
        .await;
    });
}

#[test]
fn retire_is_original_connection_only_and_never_reopens_the_lease_ledger() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (hp, cp, _, ca) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
        let mut h = ClipboardChannel::new(&l.h, hp, scope()).unwrap();
        let _c = ClipboardChannel::new(&l.c, cp, scope()).unwrap();
        let mut foreign = Link::new(&cx).await;
        let before = foreign.h.usage();
        assert_eq!(h.retire(&mut foreign.h, &cx), Err(Error::WrongRoute));
        assert_eq!(foreign.h.usage(), before);
        assert!(!foreign.h.is_closed());
        h.check(&l.h).unwrap();
        h.retire(&mut l.h, &cx).unwrap();
        peer_retired(&mut l, &cx, ca.inbound).await;
        let before = l.h.usage();
        assert!(matches!(
            offer(&mut l, &cx, 10, MediaRole::Clipboard),
            Err(Error::WrongRoute)
        ));
        assert_eq!(l.h.usage(), before);
        assert_eq!(h.retire(&mut foreign.h, &cx), Err(Error::WrongRoute));
        h.retire(&mut l.h, &cx).unwrap();
        // An unrelated media attachment is still possible; retired native IDs
        // are accounted as tombstones, not erased from the route count.
        let mut media = l.offer(&cx, 10, 2010);
        let mut peer = l.viewer(&cx, &mut media).await;
        l.attached(&cx, &mut media, &mut peer).await;
        assert!(media.is_complete());
        assert!(peer.is_complete());
    });
}

#[test]
fn complete_backpressured_receive_is_discarded_on_retirement_not_published_late() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (hp, cp, _, ca) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
        let mut h = ClipboardChannel::new(&l.h, hp, scope()).unwrap();
        let c = ClipboardChannel::new(&l.c, cp, scope()).unwrap();
        quiet(&mut l, &cx).await;
        let before = l.c.usage().framed_capacity;
        let bytes = chunk(&h);
        h.send(&mut l.h, &cx, &bytes, clock(&cx) + 1_000_000, || true)
            .unwrap();
        let until = clock(&cx) + 1_000_000;
        let mut offered = false;
        while !offered {
            assert!(clock(&cx) < until);
            l.drive(&cx).await;
            c.dispatch(
                &mut l.c,
                &cx,
                || true,
                |b| {
                    assert_eq!(b, bytes);
                    offered = true;
                    Ok(Disposition::Blocked)
                },
            )
            .unwrap();
        }
        assert!(l.c.usage().framed_capacity > before);
        h.retire(&mut l.h, &cx).unwrap();
        peer_retired(&mut l, &cx, ca.inbound).await;
        assert_eq!(l.c.usage().framed_capacity, before);
        assert!(
            c.dispatch(
                &mut l.c,
                &cx,
                || true,
                |_| panic!("old clipboard record cannot escape")
            )
            .is_err()
        );
        l.c.receive(
            &cx,
            || true,
            |_, _| panic!("retired buffer cannot re-enter generic dispatch"),
        )
        .unwrap();
        let send = l.cr.outbound;
        let receive = l.hr.inbound;
        input::transfer(
            &mut l,
            &cx,
            false,
            Route::Stream(send),
            Route::Stream(receive),
            &record(0x12, 7, 96),
        )
        .await;
    });
}

#[test]
fn simultaneous_explicit_retirement_is_idempotent_under_crossed_stop_and_reset() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (hp, cp, _, _) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
        let mut h = ClipboardChannel::new(&l.h, hp, scope()).unwrap();
        let mut c = ClipboardChannel::new(&l.c, cp, scope()).unwrap();
        quiet(&mut l, &cx).await;
        h.send(&mut l.h, &cx, &chunk(&h), clock(&cx) + 1_000_000, || true)
            .unwrap();
        c.send(&mut l.c, &cx, &chunk(&c), clock(&cx) + 1_000_000, || true)
            .unwrap();
        l.drive(&cx).await; // Prefixes now live in native/loss-recovery state.
        h.retire(&mut l.h, &cx).unwrap();
        c.retire(&mut l.c, &cx).unwrap();
        for _ in 0..12 {
            l.drive(&cx).await;
            h.retire(&mut l.h, &cx).unwrap();
            c.retire(&mut l.c, &cx).unwrap();
        }
        assert_eq!(l.h.usage().retained_send_records, 0);
        assert_eq!(l.c.usage().retained_send_records, 0);
        drop((h, c));
        l.h.tick(&cx, || true).unwrap();
        l.c.tick(&cx, || true).unwrap();
    });
}
