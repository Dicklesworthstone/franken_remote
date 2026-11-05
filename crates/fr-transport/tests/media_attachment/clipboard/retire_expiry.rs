//! Original receive expiry retires only a completed clipboard attachment.
//! These are real UDP/TLS records; no native OS permission is implied.
use super::*;
use fr_wire::stream::StreamError;

#[path = "retire_header_expiry.rs"]
mod header;

async fn block_clipboard(l: &mut Link, cx: &Cx, h: &ClipboardChannel, c: &ClipboardChannel) {
    let bytes = chunk(h);
    h.send(&mut l.h, cx, &bytes, clock(cx) + 1_000_000, || true)
        .unwrap();
    let until = clock(cx) + 1_000_000;
    let mut offered = false;
    while !offered {
        assert!(clock(cx) < until, "complete clipboard record never arrived");
        l.drive(cx).await;
        c.dispatch(
            &mut l.c,
            cx,
            || true,
            |record| {
                assert_eq!(record, bytes);
                offered = true;
                Ok(Disposition::Blocked)
            },
        )
        .unwrap();
    }
    quiet(l, cx).await;
}

#[test]
fn original_complete_record_expiry_retires_only_clipboard_and_never_renews_on_retry() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let (_hi, _ci, hi, ci) = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (hp, cp, ha, ca) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
        let h = ClipboardChannel::new(&l.h, hp, scope()).unwrap();
        let mut c = ClipboardChannel::new(&l.c, cp, scope()).unwrap();
        quiet(&mut l, &cx).await;
        let capacity = l.c.usage().framed_capacity;
        block_clipboard(&mut l, &cx, &h, &c).await;
        assert!(l.c.usage().framed_capacity > capacity);
        asupersync::time::sleep(cx.now(), Duration::from_millis(1050)).await;
        let mut retried = false;
        c.dispatch(
            &mut l.c,
            &cx,
            || true,
            |_| {
                retried = true;
                Ok(Disposition::Blocked)
            },
        )
        .unwrap();
        assert!(retried);
        asupersync::time::sleep(cx.now(), Duration::from_millis(1050)).await;
        // The default 2-second first-byte deadline, not the latest callback's
        // timestamp, retires the original complete record and its exact pair.
        l.c.tick(&cx, || true).unwrap();
        assert!(c.receive_expired(&l.c));
        assert!(
            !c.receive_expired(&l.h),
            "equal numeric IDs are not identity"
        );
        assert_eq!(l.c.usage().framed_capacity, capacity);
        assert!(!l.c.has_route(Route::Stream(ca.inbound)));
        assert!(!l.c.is_closed());
        assert_eq!(
            c.dispatch(
                &mut l.c,
                &cx,
                || true,
                |_| panic!("expired clipboard escaped")
            ),
            Err(Error::Stream(StreamError::Expired))
        );
        c.retire(&mut l.c, &cx).unwrap();
        assert!(
            c.receive_expired(&l.c),
            "idempotent cleanup retains the cause"
        );
        let until = clock(&cx) + 1_000_000;
        while l.h.has_route(Route::Stream(ha.outbound)) {
            assert!(
                clock(&cx) < until,
                "peer did not observe the optional reset"
            );
            l.drive(&cx).await;
        }
        assert!(h.receive_expired(&l.h));
        assert_eq!(h.check(&l.h), Err(Error::Stream(StreamError::Expired)));
        let (hr, cr) = (l.hr, l.cr);
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
        assert!(matches!(
            offer(&mut l, &cx, 10, MediaRole::Clipboard),
            Err(Error::WrongRoute)
        ));
        drop((h, c));
        l.h.tick(&cx, || true).unwrap();
        l.c.tick(&cx, || true).unwrap();
    });
}

async fn stalled_body(host: bool) {
    let cx = Cx::current().unwrap();
    let mut l = Link::new(&cx).await;
    enable(&mut l);
    let (_ih, _ic, hi, ci) = attach(&mut l, &cx, 8, MediaRole::Input).await;
    let (hp, cp, ha, ca) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
    let h = ClipboardChannel::new(&l.h, hp, scope()).unwrap();
    let c = ClipboardChannel::new(&l.c, cp, scope()).unwrap();
    quiet(&mut l, &cx).await;
    let capacity = sending(&mut l, !host).usage().framed_capacity;
    // Consume a previously blocked record. The following stalled body has
    // its own first-byte deadline, independent of that old callback marker.
    block_clipboard(&mut l, &cx, &h, &c).await;
    c.dispatch(&mut l.c, &cx, || true, |_| Ok(Disposition::Consumed))
        .unwrap();
    quiet(&mut l, &cx).await;
    let (sender, receiver, send_routes) = if host { (&h, &c, ha) } else { (&c, &h, ca) };
    sender
        .send(
            sending(&mut l, host),
            &cx,
            &chunk(sender),
            clock(&cx) + 4_000_000,
            || true,
        )
        .unwrap();
    // One bounded send burst leaves this large record incomplete. Do not
    // drive the sender again; there can be no successful callback here.
    sending(&mut l, host)
        .drive(&cx, Duration::ZERO, || true)
        .await
        .unwrap();
    for _ in 0..3 {
        sending(&mut l, !host)
            .drive(&cx, Duration::from_millis(1), || true)
            .await
            .unwrap();
        receiver
            .dispatch(
                sending(&mut l, !host),
                &cx,
                || true,
                |_| panic!("only a prefix was sent"),
            )
            .unwrap();
    }
    assert!(sending(&mut l, !host).usage().framed_capacity >= 7800);
    expire_partial(&mut l, &cx, host, receiver).await;
    assert_eq!(sending(&mut l, !host).usage().framed_capacity, capacity);
    let until = clock(&cx) + 1_000_000;
    while sending(&mut l, host).has_route(Route::Stream(send_routes.outbound)) {
        assert!(clock(&cx) < until, "peer never learned clipboard expiry");
        l.drive(&cx).await;
    }
    assert!(sender.receive_expired(sending(&mut l, host)));
    assert_eq!(
        sender.check(sending(&mut l, host)),
        Err(Error::Stream(StreamError::Expired))
    );
    survives(&mut l, &cx, hi, ci).await;
    assert!(matches!(
        offer(&mut l, &cx, 10, MediaRole::Clipboard),
        Err(Error::WrongRoute)
    ));
}

async fn expire_partial(l: &mut Link, cx: &Cx, host: bool, receiver: &ClipboardChannel) {
    asupersync::time::sleep(cx.now(), Duration::from_millis(1050)).await;
    receiver
        .dispatch(
            sending(l, !host),
            cx,
            || true,
            |_| panic!("partial record escaped"),
        )
        .unwrap();
    asupersync::time::sleep(cx.now(), Duration::from_millis(1050)).await;
    // Dispatch preflight must expire the original record, without giving a
    // repeated partial read a new lifetime or invoking the item handler.
    assert_eq!(
        receiver.dispatch(
            sending(l, !host),
            cx,
            || true,
            |_| panic!("expired record escaped")
        ),
        Err(Error::Stream(StreamError::Expired))
    );
    assert!(!sending(l, !host).is_closed());
    assert!(receiver.receive_expired(sending(l, !host)));
    assert!(!receiver.receive_expired(sending(l, host)));
    assert_eq!(receiver.check(sending(l, host)), Err(Error::WrongRoute));
}

fn sending(l: &mut Link, host: bool) -> &mut QuicRecords {
    if host { &mut l.h } else { &mut l.c }
}

async fn survives(l: &mut Link, cx: &Cx, hi: AttachedChannel, ci: AttachedChannel) {
    let (hr, cr) = (l.hr, l.cr);
    for (host, send, receive, kind, binding) in [
        (true, hr.outbound, cr.inbound, 0x12, 7),
        (false, cr.outbound, hr.inbound, 0x12, 7),
        (true, hi.outbound, ci.inbound, 0x17, 8),
        (false, ci.outbound, hi.inbound, 0x40, 8),
    ] {
        input::transfer(
            l,
            cx,
            host,
            Route::Stream(send),
            Route::Stream(receive),
            &record(kind, binding, 96),
        )
        .await;
    }
    assert!(!l.h.is_closed());
    assert!(!l.c.is_closed());
}

#[test]
fn incomplete_clipboard_body_expiry_preserves_control_and_input_in_both_directions() {
    for host in [false, true] {
        runtime().block_on(Box::pin(stalled_body(host)));
    }
}

#[test]
fn complete_but_blocked_critical_record_expiry_still_fences_the_connection() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        let bytes = record(0x12, 7, 96);
        l.h.send(
            &cx,
            Route::Stream(l.hr.outbound),
            &bytes,
            clock(&cx) + 1_000_000,
            || true,
        )
        .unwrap();
        let until = clock(&cx) + 1_000_000;
        let mut blocked = false;
        while !blocked {
            assert!(clock(&cx) < until);
            l.drive(&cx).await;
            l.c.receive(
                &cx,
                || true,
                |route, received| {
                    assert_eq!(route, Route::Stream(l.cr.inbound));
                    assert_eq!(received, bytes);
                    blocked = true;
                    Ok(Disposition::Blocked)
                },
            )
            .unwrap();
        }
        quiet(&mut l, &cx).await;
        asupersync::time::sleep(cx.now(), Duration::from_millis(2100)).await;
        assert_eq!(
            l.c.tick(&cx, || true),
            Err(Error::Stream(StreamError::Expired))
        );
        assert!(l.c.is_closed());
    });
}

#[test]
fn malformed_clipboard_record_still_fences_before_application_dispatch() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (hp, cp, _, ca) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
        let h = ClipboardChannel::new(&l.h, hp, scope()).unwrap();
        let _c = ClipboardChannel::new(&l.c, cp, scope()).unwrap();
        // A valid transport envelope cannot excuse an invalid clipboard scope
        // or stamp. It fails immediately, before any expiry exception applies.
        l.c.send(
            &cx,
            Route::Stream(ca.outbound),
            &record(0x50, ca.outbound.binding, 128),
            clock(&cx) + 1_000_000,
            || true,
        )
        .unwrap();
        let until = clock(&cx) + 1_000_000;
        loop {
            assert!(clock(&cx) < until);
            l.drive(&cx).await;
            match h.dispatch(
                &mut l.h,
                &cx,
                || true,
                |_| panic!("malformed clipboard escaped"),
            ) {
                Ok(()) => {}
                Err(error) => {
                    assert_eq!(error, Error::Handler);
                    break;
                }
            }
        }
        assert!(l.h.is_closed());
        assert!(!h.receive_expired(&l.h));
    });
}
