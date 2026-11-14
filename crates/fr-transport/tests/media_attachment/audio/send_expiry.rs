//! Real UDP/TLS custody for fixed reliable-send expiry on each audio direction.
//! Payload envelopes test transport ownership, not native playback or capture.
use super::expiry::{endpoint, quiet};
use super::*;
use crate::audio_up;
use fr_transport::quic::audio::AudioUpChannel;

#[allow(clippy::too_many_lines)]
async fn preserves_siblings(role: MediaRole, host: bool, staged: bool) {
    let cx = Cx::current().unwrap();
    let mut l = Link::new(&cx).await;
    audio_up::enable(&mut l);
    let (_video_h, _video_c, hv, cv) = attach(&mut l, &cx, MediaRole::Video, 8).await;
    let (_input_h, _input_c, hi, ci) = attach(&mut l, &cx, MediaRole::Input, 9).await;
    let (mut down_h, mut down_c, hd, cd) = attach(&mut l, &cx, MediaRole::AudioDown, 10).await;
    let (up_h, up_c, hu, cu) = attach(&mut l, &cx, MediaRole::AudioUp, 11).await;
    let mut up_h = AudioUpChannel::new(&l.h, up_h).unwrap();
    let mut up_c = AudioUpChannel::new(&l.c, up_c).unwrap();
    quiet(&mut l, &cx).await;
    let (h, c, sibling_h, sibling_c) = if role == MediaRole::AudioDown {
        (hd, cd, hu, cu)
    } else {
        (hu, cu, hd, cd)
    };
    let (sender, receiver) = if host { (h, c) } else { (c, h) };
    let q = endpoint(&mut l, host);
    let original = q.binding();
    let remaining = q.remaining_channel_pairs();
    let kind = if sender.outbound.messages == Messages::AudioControl {
        0x60
    } else {
        0x61
    };
    let bytes = record(kind, sender.outbound.binding, 52);
    q.send(
        &cx,
        Route::Stream(sender.outbound),
        &bytes,
        clock(&cx) + 100_000,
        || true,
    )
    .unwrap();
    if staged {
        // Complete the native send, but do not drive the peer. This is real
        // retained retransmission custody, not a synthetic expired counter.
        q.drive(&cx, Duration::ZERO, || true).await.unwrap();
    }
    assert_eq!(q.usage().retained_send_records, 1);
    assert_eq!(q.usage().retained_send_upper_bound, bytes.len());
    asupersync::time::sleep(cx.now(), Duration::from_micros(MAX_DELIVERY_ALLOWANCE_US)).await;
    q.tick(&cx, || true).unwrap();
    assert!(!q.is_closed());
    assert!(q.is_bound_to(&original));
    assert_eq!(q.audio_send_expired(sender.inbound), Ok(true));
    assert_eq!(q.audio_receive_expired(sender.inbound), Ok(false));
    assert_eq!(q.usage().retained_send_records, 0);
    assert_eq!(q.usage().retained_send_upper_bound, 0);
    assert_eq!(q.remaining_channel_pairs(), remaining);
    assert!(!q.has_route(Route::Stream(sender.outbound)));
    assert!(!q.has_route(Route::Datagram(sender.datagram.unwrap())));
    let until = clock(&cx) + 1_000_000;
    while endpoint(&mut l, !host).has_route(Route::Stream(receiver.inbound)) {
        assert!(
            clock(&cx) < until,
            "peer never learned the original audio send expiry"
        );
        l.drive(&cx).await;
    }
    assert_eq!(l.h.audio_send_expired(h.inbound), Ok(true));
    assert_eq!(l.c.audio_send_expired(c.inbound), Ok(true));
    assert_eq!(l.h.audio_receive_expired(h.inbound), Ok(false));
    assert_eq!(l.c.audio_receive_expired(c.inbound), Ok(false));
    if role == MediaRole::AudioUp {
        assert!(up_h.send_expired(&l.h));
        assert!(up_c.send_expired(&l.c));
        assert!(!up_h.send_expired(&l.c));
        assert_eq!(up_h.check(&l.c), Err(Error::WrongRoute));
        assert_eq!(up_h.check(&l.h), Err(Error::SendExpired));
        assert_eq!(
            up_c.dispatch(
                &mut l.c,
                &cx,
                || true,
                |_| true,
                |_, _, _| panic!("retired audio escaped into its consumer"),
            ),
            Err(Error::SendExpired)
        );
        up_h.retire(&mut l.h, &cx).unwrap();
        up_c.retire(&mut l.c, &cx).unwrap();
        assert_eq!(up_h.check(&l.h), Err(Error::SendExpired));
        assert_eq!(up_c.check(&l.c), Err(Error::SendExpired));
    } else {
        down_h.retire_audio(&mut l.h, &cx).unwrap();
        down_c.retire_audio(&mut l.c, &cx).unwrap();
        up_h.check(&l.h).unwrap();
        up_c.check(&l.c).unwrap();
    }
    assert_eq!(l.h.audio_send_expired(h.inbound), Ok(true));
    assert_eq!(l.c.audio_send_expired(c.inbound), Ok(true));
    for wrong in [
        h.outbound,
        l.hr.inbound,
        StreamRoute {
            binding: 99,
            ..h.inbound
        },
    ] {
        assert_eq!(l.h.audio_send_expired(wrong), Err(Error::WrongRoute));
    }
    assert!(matches!(
        l.h.offer_media_role(
            &cx,
            ChannelScope {
                control: l.hr,
                parent: parent(),
                selection: &l.selection,
            },
            ChannelRequest {
                binding: binding(12),
                ticket: Ticket(1012),
                timeout: Duration::from_secs(2),
            },
            role,
            || true,
        ),
        Err(Error::WrongRoute)
    ));
    // Original parent, input, opposite audio, and video still transfer; no
    // reattachment or grant replacement was needed to recover those lanes.
    for (from_host, outgoing, incoming, kind) in [
        (true, l.hr.outbound, l.cr.inbound, 0x12),
        (false, l.cr.outbound, l.hr.inbound, 0x12),
        (true, hi.outbound, ci.inbound, 0x17),
        (false, ci.outbound, hi.inbound, 0x40),
        (true, sibling_h.outbound, sibling_c.inbound, 0x63),
        (false, sibling_c.outbound, sibling_h.inbound, 0x63),
    ] {
        transfer(
            &mut l,
            &cx,
            from_host,
            Route::Stream(outgoing),
            Route::Stream(incoming),
            &record(kind, outgoing.binding, 52),
        )
        .await;
    }
    transfer(
        &mut l,
        &cx,
        true,
        Route::Datagram(hv.datagram.unwrap()),
        Route::Datagram(cv.datagram.unwrap()),
        &record(0x34, 8, 300),
    )
    .await;
    assert!(!l.h.is_closed() && !l.c.is_closed());
    l.h.close();
    assert_eq!(l.h.audio_send_expired(h.inbound), Err(Error::Closed));
    assert!(!up_h.send_expired(&l.h));
}

#[test]
fn queued_audio_send_expiry_preserves_control_input_video_and_opposite_audio() {
    for role in [MediaRole::AudioDown, MediaRole::AudioUp] {
        for host in [false, true] {
            runtime().block_on(Box::pin(preserves_siblings(role, host, false)));
        }
    }
}

#[test]
fn native_audio_send_expiry_preserves_control_input_video_and_opposite_audio() {
    for role in [MediaRole::AudioDown, MediaRole::AudioUp] {
        for host in [false, true] {
            runtime().block_on(Box::pin(preserves_siblings(role, host, true)));
        }
    }
}

#[test]
fn microphone_send_expiry_cannot_release_queued_speech_for_later_transmission() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        audio_up::enable(&mut l);
        let _input = attach(&mut l, &cx, MediaRole::Input, 8).await;
        let (_h, c, _, ca) = attach(&mut l, &cx, MediaRole::AudioUp, 9).await;
        let client = AudioUpChannel::new(&l.c, c).unwrap();
        quiet(&mut l, &cx).await;
        for (route, kind) in [
            (Route::Stream(ca.outbound), 0x60),
            (Route::Datagram(ca.datagram.unwrap()), 0x62),
        ] {
            l.c.send(
                &cx,
                route,
                &record(kind, 9, 52),
                clock(&cx) + 100_000,
                || true,
            )
            .unwrap();
        }
        assert!(!client.datagrams_send_drained(&l.c).unwrap());
        asupersync::time::sleep(cx.now(), Duration::from_micros(MAX_DELIVERY_ALLOWANCE_US)).await;
        assert_eq!(l.c.tick(&cx, || true), Err(Error::RetainedMicrophoneOutput));
        assert!(l.c.is_closed());
        assert!(!client.send_expired(&l.c));
        assert_eq!(client.is_retired(&l.c), Err(Error::Closed));
        assert_eq!(l.c.drive(&cx, WAIT, || true).await, Err(Error::Closed));
    });
}

#[test]
fn audio_send_expiry_inside_native_io_or_after_authority_loss_remains_terminal() {
    for role in [MediaRole::AudioDown, MediaRole::AudioUp] {
        for lose_authority in [false, true] {
            run_test!(cx, {
                let mut l = Link::new(&cx).await;
                audio_up::enable(&mut l);
                let _input = attach(&mut l, &cx, MediaRole::Input, 8).await;
                let (_h, _c, ha, _) = attach(&mut l, &cx, role, 9).await;
                quiet(&mut l, &cx).await;
                let kind = if role == MediaRole::AudioDown {
                    0x60
                } else {
                    0x61
                };
                l.h.send(
                    &cx,
                    Route::Stream(ha.outbound),
                    &record(kind, 9, 52),
                    clock(&cx) + 100_000,
                    || true,
                )
                .unwrap();
                let result = if lose_authority {
                    asupersync::time::sleep(
                        cx.now(),
                        Duration::from_micros(MAX_DELIVERY_ALLOWANCE_US),
                    )
                    .await;
                    l.h.tick(&cx, || false)
                } else {
                    let mut checks = 0;
                    let result = l
                        .h
                        .drive(&cx, Duration::ZERO, || {
                            checks += 1;
                            if checks == 3 {
                                // This native future owns a staged prefix. Expiry
                                // cannot return it to ordinary traffic or reset it.
                                std::thread::sleep(Duration::from_micros(MAX_DELIVERY_ALLOWANCE_US));
                            }
                            true
                        })
                        .await;
                    assert_eq!(checks, 3);
                    result
                };
                assert_eq!(
                    result,
                    Err(if lose_authority {
                        Error::Unauthorized
                    } else {
                        Error::Expired
                    })
                );
                assert!(l.h.is_closed());
                assert_eq!(l.h.audio_send_expired(ha.inbound), Err(Error::Closed));
            });
        }
    }
}
