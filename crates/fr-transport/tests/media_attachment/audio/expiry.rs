//! Actual UDP/TLS records at the optional audio framing boundary. Audio payload
//! semantics and native playback are exercised by their session owners, not
//! claimed by these transport-envelope fixtures.
use super::*;
use crate::audio_up;
use fr_transport::quic::audio::AudioUpChannel;
use fr_wire::stream::StreamError;

async fn quiet(l: &mut Link, cx: &Cx) {
    let until = clock(cx) + 1_000_000;
    while l.h.usage().retained_send_records != 0 || l.c.usage().retained_send_records != 0 {
        assert!(clock(cx) < until, "original records did not drain");
        l.drive(cx).await;
    }
}

fn endpoint(l: &mut Link, host: bool) -> &mut QuicRecords {
    if host { &mut l.h } else { &mut l.c }
}

async fn blocked(
    l: &mut Link,
    cx: &Cx,
    host: bool,
    send: StreamRoute,
    receive: StreamRoute,
    kind: u16,
    partial: bool,
) {
    let capacity = endpoint(l, !host).usage().framed_capacity;
    let bytes = record(kind, send.binding, if partial { 1100 } else { 52 });
    endpoint(l, host)
        .send(
            cx,
            Route::Stream(send),
            &bytes,
            clock(cx) + 4_000_000,
            || true,
        )
        .unwrap();
    if partial {
        // One critical prefix is at most 900 bytes. The declared envelope is
        // deliberately larger, so even one received prefix cannot be dispatched.
        // The incomplete payload never reaches an audio semantic decoder.
        endpoint(l, host)
            .drive(cx, Duration::ZERO, || true)
            .await
            .unwrap();
    }
    let until = clock(cx) + 1_000_000;
    let mut held = false;
    loop {
        assert!(clock(cx) < until, "audio receive never acquired its record");
        if partial {
            endpoint(l, !host).drive(cx, WAIT, || true).await.unwrap();
        } else {
            l.drive(cx).await;
        }
        endpoint(l, !host)
            .receive_ready(
                cx,
                || true,
                |r| r == Route::Stream(receive),
                |_, received| {
                    assert!(!partial, "an incomplete record was dispatched");
                    assert_eq!(received, bytes);
                    held = true;
                    Ok(Disposition::Blocked)
                },
            )
            .unwrap();
        if held || (partial && endpoint(l, !host).usage().framed_capacity > capacity) {
            break;
        }
    }
    if !partial {
        quiet(l, cx).await;
    }
}

#[allow(clippy::too_many_lines)]
async fn survives(role: MediaRole, host: bool, partial: bool) {
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
    let (send, receive) = match (role, host) {
        (MediaRole::AudioDown, true) => (hd.outbound, cd.inbound),
        (MediaRole::AudioDown, false) => (cd.outbound, hd.inbound),
        (MediaRole::AudioUp, true) => (hu.outbound, cu.inbound),
        (MediaRole::AudioUp, false) => (cu.outbound, hu.inbound),
        _ => unreachable!(),
    };
    let capacity = endpoint(&mut l, !host).usage().framed_capacity;
    let kind = if send.messages == Messages::AudioControl {
        0x60
    } else {
        0x61
    };
    blocked(&mut l, &cx, host, send, receive, kind, partial).await;
    asupersync::time::sleep(cx.now(), Duration::from_millis(2100)).await;
    // This is the production pre-dispatch check, not a synthetic tombstone.
    endpoint(&mut l, !host)
        .receive_ready(
            &cx,
            || true,
            |r| r == Route::Stream(receive),
            |_, _| panic!("expired audio reached its application"),
        )
        .unwrap();
    assert!(!endpoint(&mut l, !host).is_closed());
    assert_eq!(endpoint(&mut l, !host).usage().framed_capacity, capacity);
    assert_eq!(
        endpoint(&mut l, !host).audio_receive_expired(receive),
        Ok(true)
    );
    assert!(!endpoint(&mut l, !host).has_route(Route::Stream(receive)));
    let until = clock(&cx) + 1_000_000;
    while endpoint(&mut l, host).has_route(Route::Stream(send)) {
        assert!(clock(&cx) < until, "peer did not observe audio expiry");
        l.drive(&cx).await;
    }
    let (h, c, sibling_h, sibling_c) = if role == MediaRole::AudioDown {
        down_h.retire_audio(&mut l.h, &cx).unwrap();
        down_c.retire_audio(&mut l.c, &cx).unwrap();
        up_h.check(&l.h).unwrap();
        up_c.check(&l.c).unwrap();
        (hd, cd, hu, cu)
    } else {
        assert!(up_h.receive_expired(&l.h));
        assert!(up_c.receive_expired(&l.c));
        assert!(!up_h.receive_expired(&l.c));
        assert_eq!(up_h.check(&l.c), Err(Error::WrongRoute));
        assert_eq!(up_h.check(&l.h), Err(Error::Stream(StreamError::Expired)));
        assert_eq!(up_c.check(&l.c), Err(Error::Stream(StreamError::Expired)));
        up_h.retire(&mut l.h, &cx).unwrap();
        up_c.retire(&mut l.c, &cx).unwrap();
        (hu, cu, hd, cd)
    };
    assert_eq!(l.h.audio_receive_expired(h.inbound), Ok(true));
    assert_eq!(l.c.audio_receive_expired(c.inbound), Ok(true));
    assert_eq!(
        l.h.audio_receive_expired(l.hr.inbound),
        Err(Error::WrongRoute)
    );
    let mut foreign = Link::new(&cx).await;
    assert_eq!(
        foreign.h.audio_receive_expired(h.inbound),
        Err(Error::WrongRoute)
    );
    foreign.h.tick(&cx, || true).unwrap();
    for (sender, outgoing, incoming, kind) in [
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
            sender,
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
    assert!(!l.h.is_closed());
    assert!(!l.c.is_closed());
}

#[test]
fn blocked_audio_control_and_reply_expiry_preserve_video_input_and_opposite_audio() {
    runtime().block_on(async {
        for role in [MediaRole::AudioDown, MediaRole::AudioUp] {
            for host in [false, true] {
                Box::pin(survives(role, host, false)).await;
            }
        }
    });
}

#[test]
fn partial_audio_control_and_reply_expiry_preserve_video_input_and_opposite_audio() {
    runtime().block_on(async {
        for role in [MediaRole::AudioDown, MediaRole::AudioUp] {
            for host in [false, true] {
                Box::pin(survives(role, host, true)).await;
            }
        }
    });
}

#[test]
fn expired_microphone_reply_cannot_release_queued_speech_for_later_transmission() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        audio_up::enable(&mut l);
        let _input = attach(&mut l, &cx, MediaRole::Input, 8).await;
        let (_h, c, ha, ca) = attach(&mut l, &cx, MediaRole::AudioUp, 9).await;
        let client = AudioUpChannel::new(&l.c, c).unwrap();
        quiet(&mut l, &cx).await;
        blocked(&mut l, &cx, true, ha.outbound, ca.inbound, 0x61, false).await;
        l.c.send(
            &cx,
            Route::Datagram(ca.datagram.unwrap()),
            &record(0x62, 9, 300),
            clock(&cx) + 4_000_000,
            || true,
        )
        .unwrap();
        assert!(!client.datagrams_send_drained(&l.c).unwrap());
        asupersync::time::sleep(cx.now(), Duration::from_millis(2100)).await;
        assert_eq!(l.c.tick(&cx, || true), Err(Error::RetainedMicrophoneOutput));
        assert!(l.c.is_closed());
        assert!(!client.receive_expired(&l.c));
        assert_eq!(client.is_retired(&l.c), Err(Error::Closed));
        assert_eq!(l.c.drive(&cx, WAIT, || true).await, Err(Error::Closed));
    });
}

#[test]
fn ordinary_audio_retirement_does_not_claim_receive_expiry() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        audio_up::enable(&mut l);
        let _input = attach(&mut l, &cx, MediaRole::Input, 8).await;
        let (mut h, mut c, ha, ca) = attach(&mut l, &cx, MediaRole::AudioDown, 9).await;
        quiet(&mut l, &cx).await;
        h.retire_audio(&mut l.h, &cx).unwrap();
        let until = clock(&cx) + 1_000_000;
        while l.c.has_route(Route::Stream(ca.outbound)) {
            assert!(clock(&cx) < until);
            l.drive(&cx).await;
        }
        c.retire_audio(&mut l.c, &cx).unwrap();
        assert_eq!(l.h.audio_receive_expired(ha.inbound), Ok(false));
        assert_eq!(l.c.audio_receive_expired(ca.inbound), Ok(false));
        assert!(!l.h.is_closed());
        assert!(!l.c.is_closed());
    });
}

#[test]
fn retained_audio_send_expiry_still_closes_the_connection() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        audio_up::enable(&mut l);
        let _input = attach(&mut l, &cx, MediaRole::Input, 8).await;
        let (_h, _c, ha, _) = attach(&mut l, &cx, MediaRole::AudioDown, 9).await;
        quiet(&mut l, &cx).await;
        l.h.send(
            &cx,
            Route::Stream(ha.outbound),
            &record(0x60, 9, 52),
            clock(&cx) + 100_000,
            || true,
        )
        .unwrap();
        assert_eq!(l.h.usage().retained_send_records, 1);
        asupersync::time::sleep(cx.now(), Duration::from_micros(MAX_DELIVERY_ALLOWANCE_US)).await;
        assert_eq!(l.h.tick(&cx, || true), Err(Error::Expired));
        assert!(l.h.is_closed());
        assert_eq!(l.h.audio_receive_expired(ha.inbound), Err(Error::Closed));
    });
}

#[test]
fn unfinished_audio_attachment_expiry_cannot_retire_only_the_optional_lane() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        audio_up::enable(&mut l);
        let _input = attach(&mut l, &cx, MediaRole::Input, 8).await;
        quiet(&mut l, &cx).await;
        let selection = l.selection.clone();
        let _pending = offer_audio(&mut l, &cx, &selection).unwrap();
        asupersync::time::sleep(cx.now(), Duration::from_millis(2100)).await;
        assert_eq!(l.h.tick(&cx, || true), Err(Error::Expired));
        assert!(l.h.is_closed());
    });
}
