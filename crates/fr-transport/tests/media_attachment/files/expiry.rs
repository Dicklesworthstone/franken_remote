//! Fixed file-framing deadlines retire only the original completed file pair.
//! Real UDP/TLS transport; no filesystem publication or live tailnet is claimed.
use super::*;
use fr_wire::stream::StreamError;

fn duplex(l: &mut Link) {
    enable(l);
    for (name, version) in [
        (wire::CHANNEL_SCOPE_CAPABILITY, wire::CHANNEL_SCOPE_VERSION),
        (wire::DOWNLOAD_CAPABILITY, wire::DOWNLOAD_VERSION),
        (wire::DUPLEX_CAPABILITY, wire::DUPLEX_VERSION),
    ] {
        l.selection.capabilities.push(Capability {
            name: name.into(),
            version,
            required: true,
        });
    }
    l.selection.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
}

async fn quiet(l: &mut Link, cx: &Cx) {
    let until = clock(cx) + 1_000_000;
    while l.h.usage().retained_send_records != 0 || l.c.usage().retained_send_records != 0 {
        assert!(clock(cx) < until, "attachment records did not drain");
        l.drive(cx).await;
    }
}

fn chunk(channel: &FilesChannel) -> Vec<u8> {
    let payload = vec![0x5a; 7800];
    let mut bytes = vec![0; channel.limits().record_bytes()];
    let n = wire::encode(
        FileMessage {
            id: 1,
            body: Body::Chunk { atp: &payload },
        },
        channel.outgoing(),
        channel.limits(),
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(n);
    bytes
}

fn sending(l: &mut Link, host: bool) -> &mut QuicRecords {
    if host { &mut l.h } else { &mut l.c }
}

fn receiving(l: &mut Link, host: bool) -> &mut QuicRecords {
    sending(l, !host)
}

async fn expire(
    l: &mut Link,
    cx: &Cx,
    host: bool,
    sender: &FilesChannel,
    receiver: &FilesChannel,
    partial: bool,
) {
    sender
        .send(
            sending(l, host),
            cx,
            &chunk(sender),
            clock(cx) + 4_000_000,
            || true,
        )
        .unwrap();
    if partial {
        // One prefix cannot contain this complete record. Leave the native
        // sender stopped while the original first-byte deadline expires.
        sending(l, host)
            .drive(cx, Duration::ZERO, || true)
            .await
            .unwrap();
        let until = clock(cx) + 1_000_000;
        while receiving(l, host).usage().framed_capacity < 7800 {
            assert!(clock(cx) < until, "file prefix never reached framing");
            receiving(l, host)
                .drive(cx, Duration::from_millis(1), || true)
                .await
                .unwrap();
            receiver
                .dispatch(
                    receiving(l, host),
                    cx,
                    || true,
                    |_| panic!("only a prefix was sent"),
                )
                .unwrap();
        }
    } else {
        let until = clock(cx) + 1_000_000;
        let mut blocked = false;
        while !blocked {
            assert!(clock(cx) < until);
            l.drive(cx).await;
            receiver
                .dispatch(
                    receiving(l, host),
                    cx,
                    || true,
                    |_| {
                        blocked = true;
                        Ok(Disposition::Blocked)
                    },
                )
                .unwrap();
        }
        quiet(l, cx).await;
    }
    assert!(
        receiving(l, host).usage().framed_capacity >= 7800,
        "the actual receive prefix is retained"
    );
    asupersync::time::sleep(cx.now(), Duration::from_millis(2100)).await;
    // Exercise dispatch's preflight, not only a standalone tick. No expired
    // body may reach the application after the pair is retired.
    assert_eq!(
        receiver.dispatch(
            receiving(l, host),
            cx,
            || true,
            |_| panic!("expired file escaped")
        ),
        Err(Error::Stream(StreamError::Expired))
    );
    assert!(receiver.receive_expired(receiving(l, host)));
    assert_eq!(
        receiver.check(receiving(l, host)),
        Err(Error::Stream(StreamError::Expired))
    );
    assert!(!receiver.receive_expired(sending(l, host)));
    assert_eq!(receiver.check(sending(l, host)), Err(Error::WrongRoute));
}

async fn survives(role: MediaRole, partial: bool) {
    let cx = Cx::current().unwrap();
    let mut l = Link::new(&cx).await;
    duplex(&mut l);
    let (_ih, _ic, hi, ci) = attach(&mut l, &cx, 8, MediaRole::Input).await;
    let (hp, cp, ha, ca) = attach(&mut l, &cx, 9, MediaRole::Files).await;
    let (dhp, dcp, dha, dca) = attach(&mut l, &cx, 10, MediaRole::FilesDownload).await;
    let mut h = FilesChannel::new(&l.h, hp, InputLeaseId::from_raw(123), 456).unwrap();
    let c = FilesChannel::new(&l.c, cp, InputLeaseId::from_raw(123), 456).unwrap();
    let dh = FilesChannel::new(&l.h, dhp, InputLeaseId::from_raw(123), 789).unwrap();
    let mut dc = FilesChannel::new(&l.c, dcp, InputLeaseId::from_raw(123), 789).unwrap();
    let host = role == MediaRole::FilesDownload;
    let (sender, receiver, expired_send, expired_receive, sibling_host, sibling_client) =
        if role == MediaRole::Files {
            (&c, &mut h, ca, ha, dha, dca)
        } else {
            (&dh, &mut dc, dha, dca, ha, ca)
        };
    quiet(&mut l, &cx).await;
    let capacity = receiving(&mut l, host).usage().framed_capacity;
    expire(&mut l, &cx, host, sender, receiver, partial).await;
    assert!(!receiving(&mut l, host).is_closed());
    assert!(!receiving(&mut l, host).has_route(Route::Stream(expired_receive.inbound)));
    assert_eq!(receiving(&mut l, host).usage().framed_capacity, capacity);
    receiver.retire(receiving(&mut l, host), &cx).unwrap();
    assert!(
        receiver.receive_expired(receiving(&mut l, host)),
        "cleanup retains its terminal reason"
    );
    let until = clock(&cx) + 1_000_000;
    while sending(&mut l, host).has_route(Route::Stream(expired_send.outbound)) {
        assert!(
            clock(&cx) < until,
            "sender did not observe the retired pair"
        );
        l.drive(&cx).await;
    }
    assert!(sender.receive_expired(sending(&mut l, host)));
    assert_eq!(
        sender.check(sending(&mut l, host)),
        Err(Error::Stream(StreamError::Expired)),
        "the peer learns the original terminal transfer outcome"
    );
    let (hr, cr) = (l.hr, l.cr);
    for (host, send, receive, kind, binding) in [
        (true, hr.outbound, cr.inbound, 0x12, 7),
        (false, cr.outbound, hr.inbound, 0x12, 7),
        (true, hi.outbound, ci.inbound, 0x17, 8),
        (false, ci.outbound, hi.inbound, 0x40, 8),
        (
            true,
            sibling_host.outbound,
            sibling_client.inbound,
            0x70,
            sibling_host.outbound.binding,
        ),
        (
            false,
            sibling_client.outbound,
            sibling_host.inbound,
            0x71,
            sibling_client.outbound.binding,
        ),
    ] {
        input::transfer(
            &mut l,
            &cx,
            host,
            Route::Stream(send),
            Route::Stream(receive),
            &record(kind, binding, 128),
        )
        .await;
    }
    assert!(matches!(
        offer(&mut l, &cx, 9, role),
        Err(Error::WrongRoute)
    ));
    l.h.tick(&cx, || true).unwrap();
    l.c.tick(&cx, || true).unwrap();
}

#[test]
fn partial_file_receive_expiry_preserves_control_input_and_the_other_file_direction() {
    runtime().block_on(async {
        for role in [MediaRole::Files, MediaRole::FilesDownload] {
            Box::pin(survives(role, true)).await;
        }
    });
}

#[test]
fn blocked_file_receive_expiry_preserves_control_input_and_the_other_file_direction() {
    runtime().block_on(async {
        for role in [MediaRole::Files, MediaRole::FilesDownload] {
            Box::pin(survives(role, false)).await;
        }
    });
}

#[test]
fn ordinary_file_retirement_is_not_reported_as_receive_expiry() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (hp, cp, _, ca) = attach(&mut l, &cx, 9, MediaRole::Files).await;
        let mut h = FilesChannel::new(&l.h, hp, InputLeaseId::from_raw(123), 456).unwrap();
        let c = FilesChannel::new(&l.c, cp, InputLeaseId::from_raw(123), 456).unwrap();
        quiet(&mut l, &cx).await;
        h.retire(&mut l.h, &cx).unwrap();
        let until = clock(&cx) + 1_000_000;
        while l.c.has_route(Route::Stream(ca.outbound)) {
            assert!(clock(&cx) < until);
            l.drive(&cx).await;
        }
        assert!(!h.receive_expired(&l.h));
        assert!(!c.receive_expired(&l.c));
        assert_eq!(h.check(&l.h), Err(Error::Closed));
        assert_eq!(c.check(&l.c), Err(Error::Closed));
        assert!(!l.h.is_closed());
        assert!(!l.c.is_closed());
    });
}

#[test]
fn malformed_file_envelope_still_fences_the_connection() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (hp, cp, _, ca) = attach(&mut l, &cx, 9, MediaRole::Files).await;
        let h = FilesChannel::new(&l.h, hp, InputLeaseId::from_raw(123), 456).unwrap();
        let _c = FilesChannel::new(&l.c, cp, InputLeaseId::from_raw(123), 456).unwrap();
        quiet(&mut l, &cx).await;
        // Generic transport accepts this Files kind, but a zeroed file envelope
        // has no valid session/lease/handle. Retirement must not excuse it.
        l.c.send(
            &cx,
            Route::Stream(ca.outbound),
            &record(0x70, ca.outbound.binding, 128),
            clock(&cx) + 1_000_000,
            || true,
        )
        .unwrap();
        let until = clock(&cx) + 1_000_000;
        loop {
            assert!(clock(&cx) < until);
            l.drive(&cx).await;
            match h.dispatch(&mut l.h, &cx, || true, |_| panic!("malformed file escaped")) {
                Ok(()) => {}
                Err(error) => {
                    assert_eq!(error, Error::Handler);
                    break;
                }
            }
        }
        assert!(l.h.is_closed());
        assert!(!h.receive_expired(&l.h));
        assert_eq!(h.check(&l.h), Err(Error::Closed));
    });
}

#[test]
fn retained_file_send_expiry_still_fences_the_connection() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (hp, cp, _, _) = attach(&mut l, &cx, 9, MediaRole::Files).await;
        let _h = FilesChannel::new(&l.h, hp, InputLeaseId::from_raw(123), 456).unwrap();
        let c = FilesChannel::new(&l.c, cp, InputLeaseId::from_raw(123), 456).unwrap();
        quiet(&mut l, &cx).await;
        c.send(&mut l.c, &cx, &chunk(&c), clock(&cx) + 100_000, || true)
            .unwrap();
        asupersync::time::sleep(cx.now(), Duration::from_millis(120)).await;
        assert_eq!(l.c.tick(&cx, || true), Err(Error::Expired));
        assert!(l.c.is_closed());
        assert!(!c.receive_expired(&l.c));
        assert_eq!(c.check(&l.c), Err(Error::Closed));
    });
}
