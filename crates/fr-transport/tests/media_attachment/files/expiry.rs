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
fn unsubmitted_file_send_expiry_retires_the_original_pair() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (hp, cp, _, _) = attach(&mut l, &cx, 9, MediaRole::Files).await;
        let _h = FilesChannel::new(&l.h, hp, InputLeaseId::from_raw(123), 456).unwrap();
        let c = FilesChannel::new(&l.c, cp, InputLeaseId::from_raw(123), 456).unwrap();
        quiet(&mut l, &cx).await;
        let bytes = chunk(&c);
        c.send(&mut l.c, &cx, &bytes, clock(&cx) + 100_000, || true)
            .unwrap();
        assert_eq!(l.c.usage().retained_send_records, 1);
        assert_eq!(l.c.usage().retained_send_upper_bound, bytes.len());
        // The short send-by limits admission. Once accepted, this exact record
        // retains the path's fixed delivery allowance (250 ms to 2 seconds).
        // Keep both peers idle and pass its public upper bound from AFTER
        // admission; 120 ms was necessarily before even the minimum deadline.
        // This is still entirely application-owned queued output; retirement
        // must occur before any native send can admit the stale record.
        asupersync::time::sleep(cx.now(), Duration::from_micros(MAX_DELIVERY_ALLOWANCE_US)).await;
        l.c.tick(&cx, || true).unwrap();
        assert!(!l.c.is_closed());
        assert!(c.send_expired(&l.c));
        assert!(!c.receive_expired(&l.c));
        assert_eq!(c.check(&l.c), Err(Error::SendExpired));
        assert_eq!(l.c.usage().retained_send_records, 0);
        assert_eq!(l.c.usage().retained_send_upper_bound, 0);
    });
}

#[allow(clippy::too_many_lines)]
async fn send_expiry_preserves_siblings(role: MediaRole, staged: bool) {
    let cx = Cx::current().unwrap();
    let mut l = Link::new(&cx).await;
    duplex(&mut l);
    let (_ih, _ic, hi, ci) = attach(&mut l, &cx, 8, MediaRole::Input).await;
    let (hp, cp, ha, ca) = attach(&mut l, &cx, 9, MediaRole::Files).await;
    let (dhp, dcp, dha, dca) = attach(&mut l, &cx, 10, MediaRole::FilesDownload).await;
    let mut h = FilesChannel::new(&l.h, hp, InputLeaseId::from_raw(123), 456).unwrap();
    let mut c = FilesChannel::new(&l.c, cp, InputLeaseId::from_raw(123), 456).unwrap();
    let mut dh = FilesChannel::new(&l.h, dhp, InputLeaseId::from_raw(123), 789).unwrap();
    let mut dc = FilesChannel::new(&l.c, dcp, InputLeaseId::from_raw(123), 789).unwrap();
    let host = role == MediaRole::FilesDownload;
    let (sender, receiver, old_send, old_receive, sibling_host, sibling_client) = if host {
        (&mut dh, &mut dc, dha, dca, ha, ca)
    } else {
        (&mut c, &mut h, ca, ha, dha, dca)
    };
    quiet(&mut l, &cx).await;
    let original = sending(&mut l, host).binding();
    let remaining = sending(&mut l, host).remaining_channel_pairs();
    let bytes = chunk(sender);
    sender
        .send(
            sending(&mut l, host),
            &cx,
            &bytes,
            clock(&cx) + 100_000,
            || true,
        )
        .unwrap();
    if staged {
        // Complete one real native turn without driving the peer. At least a
        // prefix is now retained by native loss recovery. The next completed
        // operation boundary may reset that original stream, never reuse it.
        sending(&mut l, host)
            .drive(&cx, Duration::ZERO, || true)
            .await
            .unwrap();
    }
    assert_eq!(sending(&mut l, host).usage().retained_send_records, 1);
    assert_eq!(
        sending(&mut l, host).usage().retained_send_upper_bound,
        bytes.len()
    );
    asupersync::time::sleep(cx.now(), Duration::from_micros(MAX_DELIVERY_ALLOWANCE_US)).await;
    // Discover expiry inside send's final connection check. The original
    // pair's terminal cause must survive the now-missing route; no second
    // record may be admitted with a fresh deadline.
    assert_eq!(
        sender.send(
            sending(&mut l, host),
            &cx,
            &bytes,
            clock(&cx) + 1_000_000,
            || true
        ),
        Err(Error::SendExpired)
    );
    assert!(sending(&mut l, host).is_bound_to(&original));
    assert!(!sending(&mut l, host).is_closed());
    assert!(sender.send_expired(sending(&mut l, host)));
    assert!(!sender.receive_expired(sending(&mut l, host)));
    assert!(!sender.send_expired(receiving(&mut l, host)));
    assert_eq!(sender.check(receiving(&mut l, host)), Err(Error::WrongRoute));
    assert_eq!(sending(&mut l, host).usage().retained_send_records, 0);
    assert_eq!(sending(&mut l, host).usage().retained_send_upper_bound, 0);
    assert!(!sending(&mut l, host).has_route(Route::Stream(old_send.outbound)));
    assert_eq!(sending(&mut l, host).remaining_channel_pairs(), remaining);
    sender.retire(sending(&mut l, host), &cx).unwrap();
    assert_eq!(sender.check(sending(&mut l, host)), Err(Error::SendExpired));
    let until = clock(&cx) + 1_000_000;
    while receiving(&mut l, host).has_route(Route::Stream(old_receive.inbound)) {
        assert!(
            clock(&cx) < until,
            "peer did not receive the original send-expiry cause"
        );
        l.drive(&cx).await;
    }
    assert!(receiver.send_expired(receiving(&mut l, host)));
    assert!(!receiver.receive_expired(receiving(&mut l, host)));
    assert_eq!(
        receiver.check(receiving(&mut l, host)),
        Err(Error::SendExpired)
    );
    assert_eq!(
        receiver.dispatch(
            receiving(&mut l, host),
            &cx,
            || true,
            |_| panic!("retired bytes escaped")
        ),
        Err(Error::SendExpired)
    );
    receiver.retire(receiving(&mut l, host), &cx).unwrap();
    assert!(matches!(
        offer(&mut l, &cx, old_send.outbound.binding, role),
        Err(Error::WrongRoute)
    ));
    assert!(matches!(
        offer(&mut l, &cx, 11, role),
        Err(Error::WrongRoute)
    ));
    let (hr, cr) = (l.hr, l.cr);
    for (from_host, send, receive, kind) in [
        (true, hr.outbound, cr.inbound, 0x12),
        (false, cr.outbound, hr.inbound, 0x12),
        (true, hi.outbound, ci.inbound, 0x17),
        (false, ci.outbound, hi.inbound, 0x40),
        (true, sibling_host.outbound, sibling_client.inbound, 0x70),
        (false, sibling_client.outbound, sibling_host.inbound, 0x71),
    ] {
        input::transfer(
            &mut l,
            &cx,
            from_host,
            Route::Stream(send),
            Route::Stream(receive),
            &record(kind, send.binding, 128),
        )
        .await;
    }
    assert!(!l.h.is_closed() && !l.c.is_closed());
    receiving(&mut l, host).close();
    assert!(!receiver.send_expired(receiving(&mut l, host)));
    assert_eq!(receiver.check(receiving(&mut l, host)), Err(Error::Closed));
}

#[test]
fn queued_file_send_expiry_preserves_control_input_and_the_other_file_direction() {
    runtime().block_on(async {
        for role in [MediaRole::Files, MediaRole::FilesDownload] {
            Box::pin(send_expiry_preserves_siblings(role, false)).await;
        }
    });
}

#[test]
fn native_file_send_expiry_preserves_control_input_and_the_other_file_direction() {
    runtime().block_on(async {
        for role in [MediaRole::Files, MediaRole::FilesDownload] {
            Box::pin(send_expiry_preserves_siblings(role, true)).await;
        }
    });
}

#[test]
fn file_send_expiry_cannot_hide_expired_critical_output() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (hp, cp, _, _) = attach(&mut l, &cx, 9, MediaRole::Files).await;
        let _h = FilesChannel::new(&l.h, hp, InputLeaseId::from_raw(123), 456).unwrap();
        let c = FilesChannel::new(&l.c, cp, InputLeaseId::from_raw(123), 456).unwrap();
        quiet(&mut l, &cx).await;
        let until = clock(&cx) + 100_000;
        c.send(&mut l.c, &cx, &chunk(&c), until, || true)
            .unwrap();
        l.c.send(
            &cx,
            Route::Stream(l.cr.outbound),
            &record(0x12, 7, 128),
            until,
            || true,
        )
        .unwrap();
        asupersync::time::sleep(cx.now(), Duration::from_micros(MAX_DELIVERY_ALLOWANCE_US)).await;
        assert_eq!(l.c.tick(&cx, || true), Err(Error::Expired));
        assert!(l.c.is_closed());
        assert!(!c.send_expired(&l.c));
        assert_eq!(c.check(&l.c), Err(Error::Closed));
    });
}

#[test]
fn file_send_expiry_inside_a_native_poll_still_fences_the_connection() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (hp, cp, _, _) = attach(&mut l, &cx, 9, MediaRole::Files).await;
        let h = FilesChannel::new(&l.h, hp, InputLeaseId::from_raw(123), 456).unwrap();
        let c = FilesChannel::new(&l.c, cp, InputLeaseId::from_raw(123), 456).unwrap();
        quiet(&mut l, &cx).await;
        c.send(
            &mut l.c,
            &cx,
            &chunk(&c),
            clock(&cx) + 100_000,
            || true,
        )
        .unwrap();
        let mut checks = 0;
        let result = l
            .c
            .drive(&cx, Duration::ZERO, || {
                checks += 1;
                if checks == 3 {
                    // Admission and native prefix staging already succeeded. The
                    // held future still owns that prefix; no per-stream reset can
                    // prove that its private protected output has been withdrawn.
                    std::thread::sleep(Duration::from_micros(MAX_DELIVERY_ALLOWANCE_US));
                }
                true
            })
            .await;
        assert_eq!(result, Err(Error::Expired));
        assert_eq!(
            checks, 3,
            "expiry must fence before native output is polled"
        );
        assert!(l.c.is_closed());
        assert!(!c.send_expired(&l.c));
        assert_eq!(c.check(&l.c), Err(Error::Closed));
        for _ in 0..4 {
            l.h.drive(&cx, Duration::ZERO, || true).await.unwrap();
            h.dispatch(
                &mut l.h,
                &cx,
                || true,
                |_| panic!("expired native prefix escaped"),
            )
            .unwrap();
        }
    });
}

#[test]
fn missed_file_admission_deadline_is_not_retained_send_expiry() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (hp, cp, _, _) = attach(&mut l, &cx, 9, MediaRole::Files).await;
        let _h = FilesChannel::new(&l.h, hp, InputLeaseId::from_raw(123), 456).unwrap();
        let c = FilesChannel::new(&l.c, cp, InputLeaseId::from_raw(123), 456).unwrap();
        quiet(&mut l, &cx).await;
        assert_eq!(
            c.send(&mut l.c, &cx, &chunk(&c), clock(&cx), || true),
            Err(Error::Expired)
        );
        assert!(!c.send_expired(&l.c));
        assert!(!c.receive_expired(&l.c));
        c.check(&l.c).unwrap();
        assert_eq!(l.c.usage().retained_send_records, 0);
        l.c.tick(&cx, || true).unwrap();
    });
}
