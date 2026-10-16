//! Actual receive-custody state; no socket, codec or audio-device qualification.
use super::*;

fn route(binding: u32, kind: u16) -> DatagramRoute {
    DatagramRoute {
        binding,
        kind,
        outbound: false,
    }
}
fn payload(value: u8) -> Bytes {
    Bytes::from(vec![value; 1150])
}
fn pop(inbox: &mut PendingDatagrams, now: u64, expected: DatagramRoute, value: u8) {
    assert_eq!(
        inbox.receive_one(now, &mut |r, bytes| {
            assert_eq!(r, Route::Datagram(expected));
            assert_eq!(bytes, &[value; 1150]);
            Ok(Disposition::Consumed)
        }),
        Ok(1)
    );
}

#[test]
fn audio_burst_is_lossless_bounded_and_fifo_across_partial_receives() {
    let audio = route(1, AUDIO_PACKET);
    let mut inbox = PendingDatagrams::new();
    for value in 0..=u8::try_from(AUDIO_OVERFLOW).unwrap() {
        assert!(inbox.hold(audio, payload(value), 0, 200_000).unwrap());
        assert!(!inbox.waiting(0));
    }
    assert_eq!(inbox.bytes(), (AUDIO_OVERFLOW + 1) * 1150);
    assert_eq!(inbox.dropped_audio, 0);
    for value in 0..=u8::try_from(AUDIO_OVERFLOW).unwrap() {
        pop(&mut inbox, 1, audio, value);
        assert_eq!(inbox.bytes(), (AUDIO_OVERFLOW - usize::from(value)) * 1150);
    }
    assert_eq!(inbox.audio_len, 0);
    assert_eq!(inbox.dropped_audio, 0);
}

#[test]
fn a_stalled_audio_flood_cannot_park_video_or_pointer_arrivals() {
    let audio = route(1, AUDIO_PACKET);
    let video = route(2, 0x0034);
    let pointer = route(3, 0x0042);
    let mut inbox = PendingDatagrams::new();
    for _ in 0..=AUDIO_OVERFLOW {
        assert!(inbox.hold(audio, payload(1), 0, 200_000).unwrap());
    }
    for _ in 0..1024 {
        assert!(inbox.hold(audio, payload(9), 1, 200_000).unwrap());
        assert!(!inbox.waiting(1));
        assert_eq!(inbox.bytes(), (AUDIO_OVERFLOW + 1) * 1150);
    }
    assert_eq!(inbox.dropped_audio, 1024);
    for r in [video, pointer] {
        assert!(inbox.hold(r, payload(7), 1, 200_000).unwrap());
        assert_eq!(
            inbox.receive_one(1, &mut |got, bytes| {
                if got == Route::Datagram(audio) {
                    assert_eq!(bytes, &[1; 1150]);
                    Ok(Disposition::Blocked)
                } else {
                    assert_eq!(got, Route::Datagram(r));
                    assert_eq!(bytes, &[7; 1150]);
                    Ok(Disposition::Consumed)
                }
            }),
            Ok(1)
        );
    }
    assert_eq!(inbox.bytes(), (AUDIO_OVERFLOW + 1) * 1150);
    assert_eq!(inbox.dropped_audio, 1024);
}

#[test]
fn audio_overflow_never_changes_video_parking_or_original_bytes() {
    let audio = route(1, AUDIO_PACKET);
    let video = route(2, 0x0034);
    let mut inbox = PendingDatagrams::new();
    for _ in 0..=AUDIO_OVERFLOW + 1 {
        assert!(inbox.hold(audio, payload(1), 0, 200_000).unwrap());
    }
    assert!(inbox.hold(video, payload(2), 0, 200_000).unwrap());
    assert!(!inbox.hold(video, payload(3), 0, 200_000).unwrap());
    assert!(inbox.waiting(0));
    assert_eq!(
        inbox.hold(audio, payload(8), 1, 200_000),
        Err(Error::Backpressure)
    );
    for value in [2, 3] {
        assert_eq!(
            inbox.receive_one(1, &mut |r, bytes| {
                if r == Route::Datagram(audio) {
                    Ok(Disposition::Blocked)
                } else {
                    assert_eq!(r, Route::Datagram(video));
                    assert_eq!(bytes, &[value; 1150]);
                    Ok(Disposition::Consumed)
                }
            }),
            Ok(1)
        );
        assert!(!inbox.waiting(1));
    }
    assert_eq!(inbox.dropped_audio, 1);
}

#[test]
fn promotion_preserves_first_receipt_deadlines_and_never_plays_expired_audio() {
    let audio = route(1, AUDIO_PACKET);
    let mut inbox = PendingDatagrams::new();
    assert!(inbox.hold(audio, payload(1), 0, 2_000_000).unwrap());
    assert!(inbox.hold(audio, payload(2), 50_000, 2_000_000).unwrap());
    assert!(inbox.hold(audio, payload(3), 75_000, 10_000).unwrap());
    assert_eq!(inbox.bytes(), 3 * 1150);
    inbox.expire(85_000);
    assert_eq!(inbox.bytes(), 2 * 1150);
    assert_eq!(inbox.dropped_audio, 1);
    inbox.expire(100_000);
    assert_eq!(inbox.bytes(), 1150);
    assert_eq!(inbox.dropped_audio, 2);
    assert_eq!(inbox.audio_len, 0);
    assert_eq!(
        inbox.receive_one(149_999, &mut |_, bytes| {
            assert_eq!(bytes, &[2; 1150]);
            Ok(Disposition::Blocked)
        }),
        Ok(0)
    );
    assert_eq!(
        inbox.receive_one(150_000, &mut |_, _| panic!("expired audio")),
        Ok(0)
    );
    assert_eq!(inbox.dropped_audio, 3);
    assert_eq!(inbox.bytes(), 0);
}

#[test]
fn multiple_audio_bindings_share_one_overflow_budget_without_cross_binding_delivery() {
    let a = route(1, AUDIO_PACKET);
    let b = route(2, AUDIO_PACKET);
    let mut inbox = PendingDatagrams::new();
    assert!(inbox.hold(a, payload(1), 0, 1000).unwrap());
    assert!(inbox.hold(b, payload(2), 0, 1000).unwrap());
    for (r, value) in [(a, 3), (b, 4), (a, 5), (b, 6), (a, 7)] {
        assert!(inbox.hold(r, payload(value), 0, 1000).unwrap());
    }
    assert_eq!(inbox.audio_len, AUDIO_OVERFLOW);
    assert_eq!(inbox.bytes(), (2 + AUDIO_OVERFLOW) * 1150);
    assert_eq!(inbox.dropped_audio, 1);
    inbox.retire(1);
    assert_eq!(inbox.bytes(), 3 * 1150);
    for value in [2, 4, 6] {
        pop(&mut inbox, 1, b, value);
    }
    assert_eq!(inbox.bytes(), 0);
    assert_eq!(inbox.dropped_audio, 1);
}

#[test]
fn removing_interior_expiry_keeps_later_audio_in_arrival_order() {
    let audio = route(1, AUDIO_PACKET);
    let mut inbox = PendingDatagrams::new();
    for (value, until) in [(1, 1000), (2, 1000), (3, 1), (4, 1000), (5, 1)] {
        assert!(inbox.hold(audio, payload(value), 0, until).unwrap());
    }
    inbox.expire(1);
    assert_eq!(inbox.dropped_audio, 2);
    assert!(inbox.hold(audio, payload(6), 1, 1000).unwrap());
    for value in [1, 2, 4, 6] {
        pop(&mut inbox, 2, audio, value);
    }
    assert_eq!(inbox.bytes(), 0);
}

#[test]
fn retirement_and_close_discard_audio_without_reusing_or_erasing_drop_evidence() {
    let audio = route(1, AUDIO_PACKET);
    let mut inbox = PendingDatagrams::new();
    for _ in 0..=AUDIO_OVERFLOW + 1 {
        assert!(inbox.hold(audio, payload(1), 0, 200_000).unwrap());
    }
    let before = inbox.bytes();
    assert_eq!(
        inbox.receive_one(1, &mut |_, _| Err(())),
        Err(Error::Handler)
    );
    assert_eq!(inbox.bytes(), before);
    inbox.clear();
    assert_eq!(inbox.bytes(), 0);
    assert_eq!(inbox.audio_len, 0);
    assert_eq!(inbox.dropped_audio, 1);
    assert_eq!(
        inbox.receive_one(1, &mut |_, _| panic!("cleared audio")),
        Ok(0)
    );
    // Saturation is content-free accounting, never a wrap into a smaller count.
    inbox.dropped_audio = u64::MAX;
    for _ in 0..=AUDIO_OVERFLOW + 1 {
        assert!(inbox.hold(audio, payload(2), 1, 10).unwrap());
    }
    inbox.expire(11);
    assert_eq!(inbox.dropped_audio, u64::MAX);
}
