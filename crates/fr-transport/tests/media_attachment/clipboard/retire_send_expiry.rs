//! Fixed retained-send expiry retires the original completed clipboard pair.
//! Real UDP/TLS continuity and custody; no native OS publication is implied.
use super::*;

fn sending(l: &mut Link, host: bool) -> &mut QuicRecords {
    if host { &mut l.h } else { &mut l.c }
}

#[allow(clippy::too_many_lines)]
async fn preserves_siblings(host: bool, staged: bool) {
    let cx = Cx::current().unwrap();
    let mut l = Link::new(&cx).await;
    enable(&mut l);
    for (name, version) in [
        (attachment::FILES_CAPABILITY, attachment::FILES_VERSION),
        (fr_wire::files::CAPABILITY, fr_wire::files::VERSION),
    ] {
        l.selection.capabilities.push(Capability {
            name: name.into(),
            version,
            required: true,
        });
    }
    l.selection.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    let (_ih, _ic, hi, ci) = attach(&mut l, &cx, 8, MediaRole::Input).await;
    let (hp, cp, ha, ca) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
    let (_fh, _fc, hf, cf) = attach(&mut l, &cx, 10, MediaRole::Files).await;
    let mut h = ClipboardChannel::new(&l.h, hp, scope()).unwrap();
    let mut c = ClipboardChannel::new(&l.c, cp, scope()).unwrap();
    let (sender, receiver, old_send, old_receive) = if host {
        (&mut h, &mut c, ha, ca)
    } else {
        (&mut c, &mut h, ca, ha)
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
        // Complete native I/O without driving the peer. The original send has
        // real unacknowledged native prefixes, with no held protected packet.
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
    if staged {
        assert_eq!(
            sender.send_prepared(
                sending(&mut l, host),
                &cx,
                &bytes,
                clock(&cx) + 1_000_000,
                || true,
                || Ok::<(), core::convert::Infallible>(()),
            ),
            Err(Error::SendExpired)
        );
    } else {
        assert_eq!(
            sender.send(
                sending(&mut l, host),
                &cx,
                &bytes,
                clock(&cx) + 1_000_000,
                || true,
            ),
            Err(Error::SendExpired)
        );
    }
    assert!(sending(&mut l, host).is_bound_to(&original));
    assert!(!sending(&mut l, host).is_closed());
    assert!(sender.send_expired(sending(&mut l, host)));
    assert!(!sender.receive_expired(sending(&mut l, host)));
    assert!(!sender.send_expired(sending(&mut l, !host)));
    assert_eq!(sender.check(sending(&mut l, !host)), Err(Error::WrongRoute));
    assert_eq!(sending(&mut l, host).usage().retained_send_records, 0);
    assert_eq!(sending(&mut l, host).usage().retained_send_upper_bound, 0);
    assert_eq!(sending(&mut l, host).remaining_channel_pairs(), remaining);
    assert!(!sending(&mut l, host).has_route(Route::Stream(old_send.outbound)));
    sender.retire(sending(&mut l, host), &cx).unwrap();
    assert_eq!(sender.check(sending(&mut l, host)), Err(Error::SendExpired));
    let until = clock(&cx) + 1_000_000;
    while sending(&mut l, !host).has_route(Route::Stream(old_receive.inbound)) {
        assert!(clock(&cx) < until, "peer did not learn original send expiry");
        l.drive(&cx).await;
    }
    assert!(receiver.send_expired(sending(&mut l, !host)));
    assert!(!receiver.receive_expired(sending(&mut l, !host)));
    assert_eq!(
        receiver.dispatch(
            sending(&mut l, !host),
            &cx,
            || true,
            |_| panic!("retired clipboard bytes escaped"),
        ),
        Err(Error::SendExpired)
    );
    receiver.retire(sending(&mut l, !host), &cx).unwrap();
    assert_eq!(
        receiver.check(sending(&mut l, !host)),
        Err(Error::SendExpired)
    );
    assert!(matches!(
        offer(&mut l, &cx, old_send.outbound.binding, MediaRole::Clipboard),
        Err(Error::WrongRoute)
    ));
    assert!(matches!(
        offer(&mut l, &cx, 11, MediaRole::Clipboard),
        Err(Error::WrongRoute)
    ));
    let (hr, cr) = (l.hr, l.cr);
    for (from_host, send, receive, kind) in [
        (true, hr.outbound, cr.inbound, 0x12),
        (false, cr.outbound, hr.inbound, 0x12),
        (true, hi.outbound, ci.inbound, 0x17),
        (false, ci.outbound, hi.inbound, 0x40),
        (true, hf.outbound, cf.inbound, 0x70),
        (false, cf.outbound, hf.inbound, 0x71),
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
    sending(&mut l, !host).close();
    assert!(!receiver.send_expired(sending(&mut l, !host)));
}

#[test]
fn queued_clipboard_send_expiry_preserves_control_input_and_files() {
    for host in [false, true] {
        runtime().block_on(Box::pin(preserves_siblings(host, false)));
    }
}

#[test]
fn native_clipboard_send_expiry_preserves_control_input_and_files() {
    for host in [false, true] {
        runtime().block_on(Box::pin(preserves_siblings(host, true)));
    }
}

#[test]
fn prepared_refusal_cannot_hide_original_retained_clipboard_expiry() {
    for refused_at in [1, 2] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut l = Link::new(&cx).await;
            enable(&mut l);
            let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
            let (hp, cp, _, ca) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
            let h = ClipboardChannel::new(&l.h, hp, scope()).unwrap();
            let c = ClipboardChannel::new(&l.c, cp, scope()).unwrap();
            quiet(&mut l, &cx).await;
            let bytes = chunk(&h);
            h.send(&mut l.h, &cx, &bytes, clock(&cx) + 100_000, || true)
                .unwrap();
            let mut prepared = 0;
            assert_eq!(
                h.send_prepared(
                    &mut l.h,
                    &cx,
                    &bytes,
                    clock(&cx) + 4_000_000,
                    || true,
                    || {
                        prepared += 1;
                        if prepared == refused_at {
                            // Test-only scheduling stall before the final
                            // connection check. Both prepared-refusal paths
                            // must retain the already-admitted send's cause.
                            std::thread::sleep(Duration::from_micros(
                                MAX_DELIVERY_ALLOWANCE_US,
                            ));
                            Err(7_u8)
                        } else {
                            Ok(())
                        }
                    },
                ),
                Err(Error::SendExpired)
            );
            assert_eq!(prepared, refused_at);
            assert!(h.send_expired(&l.h));
            assert!(!l.h.is_closed());
            assert_eq!(l.h.usage().retained_send_records, 0);
            assert_eq!(l.h.usage().retained_send_upper_bound, 0);
            peer_retired(&mut l, &cx, ca.inbound).await;
            assert_eq!(
                c.dispatch(
                    &mut l.c,
                    &cx,
                    || true,
                    |_| panic!("refused or expired record escaped"),
                ),
                Err(Error::SendExpired)
            );
        });
    }
}

#[test]
fn clipboard_send_expiry_inside_a_native_poll_still_fences_the_connection() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (hp, cp, _, _) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
        let h = ClipboardChannel::new(&l.h, hp, scope()).unwrap();
        let c = ClipboardChannel::new(&l.c, cp, scope()).unwrap();
        quiet(&mut l, &cx).await;
        h.send(&mut l.h, &cx, &chunk(&h), clock(&cx) + 100_000, || true)
            .unwrap();
        let mut checks = 0;
        let result =
            l.h.drive(&cx, Duration::ZERO, || {
                checks += 1;
                if checks == 3 {
                    // The held native future already owns the staged prefix.
                    // A stream reset cannot withdraw its protected output.
                    std::thread::sleep(Duration::from_micros(MAX_DELIVERY_ALLOWANCE_US));
                }
                true
            })
            .await;
        assert_eq!(result, Err(Error::Expired));
        assert_eq!(checks, 3, "deadline must fence before native output");
        assert!(l.h.is_closed());
        assert!(!h.send_expired(&l.h));
        for _ in 0..4 {
            l.c.drive(&cx, Duration::ZERO, || true).await.unwrap();
            c.dispatch(
                &mut l.c,
                &cx,
                || true,
                |_| panic!("expired native prefix escaped"),
            )
            .unwrap();
        }
    });
}

#[test]
fn clipboard_send_expiry_cannot_conceal_authority_loss_or_expired_control() {
    for lose_authority in [false, true] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut l = Link::new(&cx).await;
            enable(&mut l);
            let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
            let (hp, cp, _, _) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
            let h = ClipboardChannel::new(&l.h, hp, scope()).unwrap();
            let _c = ClipboardChannel::new(&l.c, cp, scope()).unwrap();
            quiet(&mut l, &cx).await;
            let until = clock(&cx) + 100_000;
            h.send(&mut l.h, &cx, &chunk(&h), until, || true)
                .unwrap();
            if !lose_authority {
                l.h.send(
                    &cx,
                    Route::Stream(l.hr.outbound),
                    &record(0x12, 7, 128),
                    until,
                    || true,
                )
                .unwrap();
            }
            asupersync::time::sleep(
                cx.now(),
                Duration::from_micros(MAX_DELIVERY_ALLOWANCE_US),
            )
            .await;
            assert_eq!(
                l.h.tick(&cx, || !lose_authority),
                Err(if lose_authority {
                    Error::Unauthorized
                } else {
                    Error::Expired
                })
            );
            assert!(l.h.is_closed());
            assert!(!h.send_expired(&l.h));
        });
    }
}

#[test]
fn clipboard_admission_expiry_does_not_retire_a_live_pair() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (hp, cp, _, _) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
        let h = ClipboardChannel::new(&l.h, hp, scope()).unwrap();
        let _c = ClipboardChannel::new(&l.c, cp, scope()).unwrap();
        quiet(&mut l, &cx).await;
        assert_eq!(
            h.send(&mut l.h, &cx, &chunk(&h), clock(&cx), || true),
            Err(Error::Expired)
        );
        assert_eq!(
            h.send_prepared(
                &mut l.h,
                &cx,
                &chunk(&h),
                clock(&cx),
                || true,
                || Ok::<(), core::convert::Infallible>(()),
            ),
            Err(Error::Expired)
        );
        assert!(!h.send_expired(&l.h));
        h.check(&l.h).unwrap();
        assert_eq!(l.h.usage().retained_send_records, 0);
        l.h.tick(&cx, || true).unwrap();
    });
}
